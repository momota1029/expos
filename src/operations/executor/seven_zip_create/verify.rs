use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use flate2::{Decompress, FlushDecompress, Status};
use sha2::Digest;

const SIGNATURE: &[u8; 6] = b"7z\xBC\xAF'\x1C";
const MAX_ENTRIES: usize = 1024;
const MAX_DEPTH: usize = 128;
const MAX_NAMES: usize = 2 * 1024 * 1024;
const MAX_HEADER: usize = 8 * 1024 * 1024;
const IO_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ManifestEntry {
    pub name: String,
    pub kind: EntryKind,
    pub length: u64,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum VerifyError {
    Cancelled,
    Deadline,
    Io,
    Invalid(&'static str),
    Mismatch,
}

type Result<T> = std::result::Result<T, VerifyError>;

#[derive(Clone, Debug)]
struct Folder {
    unpacked: u64,
    packed: u64,
    crc: Option<u32>,
}

#[derive(Default)]
struct Streams {
    pack_pos: Option<u64>,
    packs: Vec<u64>,
    pack_crcs: Vec<Option<u32>>,
    folders: Vec<Folder>,
    saw_pack_info: bool,
    saw_unpack_info: bool,
    saw_substreams: bool,
}

#[derive(Clone, Default)]
struct Files {
    names: Vec<String>,
    empty_stream: Vec<bool>,
    empty_file: Vec<bool>,
    dirs: Vec<bool>,
}

fn check(cancel: &AtomicBool, deadline: Instant) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(VerifyError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(VerifyError::Deadline);
    }
    Ok(())
}

pub(super) fn verify(
    file: &mut File,
    manifest: &[ManifestEntry],
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<()> {
    verify_reader(file, manifest, deadline, cancel)
}

fn verify_reader<R: Read + Seek>(
    reader: &mut R,
    manifest: &[ManifestEntry],
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<()> {
    check(cancel, deadline)?;
    let payload_total = validate_manifest(manifest)?;
    let max_size = payload_total
        .checked_mul(2)
        .and_then(|value| value.checked_add((8 * 1024 * 1024 + 32) as u64))
        .ok_or(VerifyError::Invalid("manifest size overflow"))?;
    let file_len = reader.seek(SeekFrom::End(0)).map_err(|_| VerifyError::Io)?;
    if file_len > max_size || file_len < 32 {
        return Err(VerifyError::Invalid("archive size outside bound"));
    }
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| VerifyError::Io)?;
    let mut sig = [0u8; 32];
    read_checked(reader, &mut sig, cancel, deadline)?;
    if &sig[..6] != SIGNATURE || sig[6] != 0 || sig[7] != 4 {
        return Err(VerifyError::Invalid("unsupported signature or 7z version"));
    }
    if crc32fast::hash(&sig[12..32]) != u32::from_le_bytes(sig[8..12].try_into().unwrap()) {
        return Err(VerifyError::Invalid("start header CRC"));
    }
    let next_offset = u64::from_le_bytes(sig[12..20].try_into().unwrap());
    let next_size = u64::from_le_bytes(sig[20..28].try_into().unwrap());
    let next_crc = u32::from_le_bytes(sig[28..32].try_into().unwrap());
    let next_start = 32u64
        .checked_add(next_offset)
        .ok_or(VerifyError::Invalid("offset overflow"))?;
    let next_end = next_start
        .checked_add(next_size)
        .ok_or(VerifyError::Invalid("header range overflow"))?;
    if next_end != file_len || next_size == 0 || next_size > MAX_HEADER as u64 {
        return Err(VerifyError::Invalid("header range or size"));
    }
    let mut header =
        vec![0; usize::try_from(next_size).map_err(|_| VerifyError::Invalid("header too large"))?];
    reader
        .seek(SeekFrom::Start(next_start))
        .map_err(|_| VerifyError::Io)?;
    read_checked(reader, &mut header, cancel, deadline)?;
    if crc32fast::hash(&header) != next_crc {
        return Err(VerifyError::Invalid("next header CRC"));
    }
    let (streams, files) = parse_header(&header)?;
    compare_entries(manifest, &streams, &files)?;
    verify_packed(
        reader,
        next_start,
        payload_total,
        &streams,
        &files,
        manifest,
        cancel,
        deadline,
    )?;
    check(cancel, deadline)?;
    Ok(())
}

fn read_checked<R: Read>(
    r: &mut R,
    bytes: &mut [u8],
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    check(cancel, deadline)?;
    r.read_exact(bytes).map_err(|_| VerifyError::Io)
}

fn validate_manifest(entries: &[ManifestEntry]) -> Result<u64> {
    if entries.len() > MAX_ENTRIES {
        return Err(VerifyError::Invalid("too many manifest entries"));
    }
    let mut names: Vec<&str> = Vec::with_capacity(entries.len());
    let mut total_names = 0usize;
    let mut payload = 0u64;
    for entry in entries {
        validate_name(&entry.name)?;
        total_names = total_names
            .checked_add(entry.name.len())
            .ok_or(VerifyError::Invalid("name budget overflow"))?;
        if total_names > MAX_NAMES {
            return Err(VerifyError::Invalid("name budget"));
        }
        if names.iter().any(|name| ordinal_equal(name, &entry.name)) {
            return Err(VerifyError::Invalid("duplicate or case-colliding name"));
        }
        names.push(&entry.name);
        if entry.kind == EntryKind::Directory && (entry.length != 0 || entry.sha256 != [0; 32]) {
            return Err(VerifyError::Invalid("directory manifest carries file data"));
        }
        if entry.kind == EntryKind::File
            && entry.length == 0
            && entry.sha256 != <[u8; 32]>::from(sha2::Sha256::digest([]))
        {
            return Err(VerifyError::Invalid("empty file digest"));
        }
        if entry.kind == EntryKind::File {
            payload = payload
                .checked_add(entry.length)
                .ok_or(VerifyError::Invalid("payload overflow"))?;
        }
    }
    for entry in entries {
        if let Some((parent, _)) = entry.name.rsplit_once('/') {
            let mut current = String::new();
            for component in parent.split('/') {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(component);
                let parent_entry = entries.iter().find(|candidate| candidate.name == current);
                if !parent_entry.is_some_and(|candidate| candidate.kind == EntryKind::Directory) {
                    return Err(VerifyError::Invalid("file used as parent"));
                }
            }
        }
    }
    Ok(payload)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAMES
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains('\\')
    {
        return Err(VerifyError::Invalid("unsafe relative name"));
    }
    let depth = name.split('/').count();
    if depth > MAX_DEPTH {
        return Err(VerifyError::Invalid("name depth"));
    }
    for component in name.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with(' ')
            || component.ends_with('.')
        {
            return Err(VerifyError::Invalid("unsafe path component"));
        }
        if component
            .chars()
            .any(|ch| ch == '\0' || ch < ' ' || "<>:\"|?*".contains(ch))
        {
            return Err(VerifyError::Invalid("invalid Windows filename character"));
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "COM¹"
                | "COM²"
                | "COM³"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
                | "LPT¹"
                | "LPT²"
                | "LPT³"
        ) {
            return Err(VerifyError::Invalid("reserved Windows device name"));
        }
    }
    Ok(())
}

fn ordinal_equal(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    {
        let a = a.encode_utf16().collect::<Vec<_>>();
        let b = b.encode_utf16().collect::<Vec<_>>();
        unsafe {
            windows::Win32::Globalization::CompareStringOrdinal(
                &a,
                &b,
                windows::Win32::Foundation::BOOL(1),
            )
        }
        .0 == 2
    }
    #[cfg(not(windows))]
    {
        a.eq_ignore_ascii_case(b)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .bytes
            .get(self.at)
            .ok_or(VerifyError::Invalid("truncated header"))?;
        self.at += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(VerifyError::Invalid("header arithmetic overflow"))?;
        let part = self
            .bytes
            .get(self.at..end)
            .ok_or(VerifyError::Invalid("truncated header"))?;
        self.at = end;
        Ok(part)
    }
    fn var(&mut self) -> Result<u64> {
        let first = self.byte()?;
        let mut value = 0u64;
        for extra in 0..8 {
            let mask = 0x80 >> extra;
            if first & mask == 0 {
                value |= u64::from(first & (mask - 1)) << (8 * extra);
                return Ok(value);
            }
            value |= u64::from(self.byte()?) << (8 * extra);
        }
        Ok(value)
    }
    fn usize_var(&mut self) -> Result<usize> {
        usize::try_from(self.var()?).map_err(|_| VerifyError::Invalid("count overflow"))
    }
    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

fn parse_header(data: &[u8]) -> Result<(Streams, Files)> {
    if data.len() > MAX_HEADER {
        return Err(VerifyError::Invalid("header budget"));
    }
    let mut c = Cursor::new(data);
    if c.byte()? != 1 {
        return Err(VerifyError::Invalid("encoded or non-header next header"));
    }
    let mut streams = Streams::default();
    let mut files = Files::default();
    let mut saw_streams = false;
    let mut saw_files = false;
    let mut saw_archive_properties = false;
    let mut section_order = 0u8;
    loop {
        match c.byte()? {
            0 => break,
            2 if !saw_archive_properties && section_order == 0 => {
                skip_properties(&mut c)?;
                saw_archive_properties = true;
            }
            4 if !saw_streams && section_order <= 1 => {
                parse_streams(&mut c, &mut streams)?;
                saw_streams = true;
                section_order = 2;
            }
            5 if !saw_files && section_order <= 2 => {
                parse_files(&mut c, &mut files)?;
                saw_files = true;
                section_order = 3;
            }
            3 | 6 | 7 | 8 | 0x17 => return Err(VerifyError::Invalid("unsupported header profile")),
            _ => return Err(VerifyError::Invalid("unknown or duplicate header section")),
        }
    }
    if !c.done() {
        return Err(VerifyError::Invalid("trailing header bytes"));
    }
    if !saw_files {
        files = Files {
            names: Vec::new(),
            empty_stream: Vec::new(),
            empty_file: Vec::new(),
            dirs: Vec::new(),
        };
    }
    Ok((streams, files))
}

fn skip_properties(c: &mut Cursor<'_>) -> Result<()> {
    loop {
        let id = c.byte()?;
        if id == 0 {
            return Ok(());
        }
        let n = c.usize_var()?;
        if n > MAX_HEADER {
            return Err(VerifyError::Invalid("archive property too large"));
        }
        if id != 0x19 || c.take(n)?.iter().any(|byte| *byte != 0) {
            return Err(VerifyError::Invalid("unsupported archive property"));
        }
    }
}

fn parse_streams(c: &mut Cursor<'_>, s: &mut Streams) -> Result<()> {
    loop {
        match c.byte()? {
            0 => return Ok(()),
            6 if !s.saw_pack_info && !s.saw_unpack_info && !s.saw_substreams => {
                s.saw_pack_info = true;
                parse_pack_info(c, s)?;
            }
            7 if s.saw_pack_info && !s.saw_unpack_info && !s.saw_substreams => {
                s.saw_unpack_info = true;
                parse_unpack_info(c, s)?;
            }
            8 if s.saw_unpack_info && !s.saw_substreams => {
                s.saw_substreams = true;
                parse_substreams(c, s)?;
            }
            _ => {
                return Err(VerifyError::Invalid(
                    "unsupported or duplicate stream section",
                ));
            }
        }
    }
}

fn parse_pack_info(c: &mut Cursor<'_>, s: &mut Streams) -> Result<()> {
    s.pack_pos = Some(c.var()?);
    let count = c.usize_var()?;
    if count > MAX_ENTRIES {
        return Err(VerifyError::Invalid("pack stream count"));
    }
    let mut saw_sizes = false;
    let mut crcs = vec![None; count];
    let mut saw_crcs = false;
    loop {
        match c.byte()? {
            0 => break,
            9 if !saw_sizes => {
                for _ in 0..count {
                    s.packs.push(c.var()?);
                }
                saw_sizes = true;
            }
            0x0a if !saw_crcs => {
                crcs = read_crc_vector(c, count)?;
                saw_crcs = true;
            }
            _ => return Err(VerifyError::Invalid("unsupported pack property")),
        }
    }
    if !saw_sizes {
        return Err(VerifyError::Invalid("missing pack sizes"));
    }
    s.pack_crcs = crcs;
    Ok(())
}

fn read_crc_vector(c: &mut Cursor<'_>, count: usize) -> Result<Vec<Option<u32>>> {
    let all = c.byte()?;
    let defined = if all == 0 {
        read_bits(c, count)?
    } else if all == 1 {
        vec![true; count]
    } else {
        return Err(VerifyError::Invalid("CRC all-defined flag"));
    };
    let mut values = vec![None; count];
    for (i, yes) in defined.into_iter().enumerate() {
        if yes {
            values[i] = Some(u32::from_le_bytes(
                c.take(4)?
                    .try_into()
                    .map_err(|_| VerifyError::Invalid("CRC width"))?,
            ));
        }
    }
    Ok(values)
}

fn read_bits(c: &mut Cursor<'_>, count: usize) -> Result<Vec<bool>> {
    let byte_count = count
        .checked_add(7)
        .ok_or(VerifyError::Invalid("bit count overflow"))?
        / 8;
    let bits = c.take(byte_count)?;
    if count % 8 != 0
        && bits
            .last()
            .is_some_and(|last| last & ((1 << (8 - count % 8)) - 1) != 0)
    {
        return Err(VerifyError::Invalid("nonzero bitset padding"));
    }
    Ok((0..count)
        .map(|i| bits[i / 8] & (0x80 >> (i % 8)) != 0)
        .collect())
}

fn parse_unpack_info(c: &mut Cursor<'_>, s: &mut Streams) -> Result<()> {
    if c.byte()? != 0x0b {
        return Err(VerifyError::Invalid("missing folder records"));
    }
    let count = c.usize_var()?;
    if count == 0 || count > MAX_ENTRIES {
        return Err(VerifyError::Invalid("folder count"));
    }
    if c.byte()? != 0 {
        return Err(VerifyError::Invalid("external folders"));
    }
    let mut coders = Vec::with_capacity(count);
    for _ in 0..count {
        let coder_count = c.usize_var()?;
        if coder_count != 1 {
            return Err(VerifyError::Invalid("multiple coders"));
        }
        let flags = c.byte()?;
        if flags != 3 {
            return Err(VerifyError::Invalid("complex coder"));
        }
        let id_len = usize::from(flags & 0x0f);
        if id_len != 3 || c.take(id_len)? != [0x04, 0x01, 0x08] {
            return Err(VerifyError::Invalid("not raw Deflate"));
        }
        if flags & 0x20 != 0 {
            return Err(VerifyError::Invalid("Deflate coder properties"));
        }
        coders.push(Folder {
            unpacked: 0,
            packed: 0,
            crc: None,
        });
    }
    if c.byte()? != 0x0c {
        return Err(VerifyError::Invalid("missing unpack sizes"));
    }
    for folder in &mut coders {
        folder.unpacked = c.var()?;
    }
    let mut saw_crc = false;
    loop {
        match c.byte()? {
            0 => break,
            0x0a if !saw_crc => {
                let crcs = read_crc_vector(c, count)?;
                for (f, crc) in coders.iter_mut().zip(crcs) {
                    f.crc = crc;
                }
                saw_crc = true;
            }
            _ => return Err(VerifyError::Invalid("unsupported unpack property")),
        }
    }
    s.folders = coders;
    Ok(())
}

fn parse_substreams(c: &mut Cursor<'_>, s: &mut Streams) -> Result<()> {
    let mut counts = vec![1usize; s.folders.len()];
    let mut sizes = Vec::new();
    let mut crcs: Option<Vec<Option<u32>>> = None;
    let mut seen = HashSet::new();
    loop {
        let property = c.byte()?;
        if property != 0 && !seen.insert(property) {
            return Err(VerifyError::Invalid("duplicate substream property"));
        }
        match property {
            0 => break,
            0x0d => {
                for count in &mut counts {
                    *count = c.usize_var()?;
                    if *count > 1 {
                        return Err(VerifyError::Invalid("multiple substreams per folder"));
                    }
                }
            }
            9 => {
                for (i, folder) in s.folders.iter().enumerate() {
                    if counts[i] == 1 {
                        sizes.push(folder.unpacked);
                    } else {
                        return Err(VerifyError::Invalid("empty folder stream"));
                    }
                }
            }
            0x0a => {
                let digest_folders = s
                    .folders
                    .iter()
                    .enumerate()
                    .filter(|(index, folder)| counts[*index] != 1 || folder.crc.is_none())
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                let values = read_crc_vector(c, digest_folders.len())?;
                let mut expanded = vec![None; s.folders.len()];
                for (index, value) in digest_folders.into_iter().zip(values) {
                    expanded[index] = value;
                }
                crcs = Some(expanded);
            }
            _ => return Err(VerifyError::Invalid("unsupported substream property")),
        }
    }
    if counts.len() != s.folders.len() || counts.iter().any(|count| *count != 1) {
        return Err(VerifyError::Invalid("substream profile"));
    }
    if !sizes.is_empty() && sizes.len() != s.folders.len() {
        return Err(VerifyError::Invalid("substream sizes"));
    }
    for (i, folder) in s.folders.iter_mut().enumerate() {
        if !sizes.is_empty() && sizes[i] != folder.unpacked {
            return Err(VerifyError::Invalid("substream size mismatch"));
        }
        if let Some(crcs) = &crcs {
            folder.crc = crcs.get(i).copied().flatten().or(folder.crc);
        }
    }
    Ok(())
}

fn parse_files(c: &mut Cursor<'_>, files: &mut Files) -> Result<()> {
    let count = c.usize_var()?;
    if count > MAX_ENTRIES {
        return Err(VerifyError::Invalid("file entry count"));
    }
    files.names = Vec::with_capacity(count);
    files.empty_stream = vec![false; count];
    files.empty_file = vec![false; count];
    files.dirs = vec![false; count];
    let mut saw_empty_stream = false;
    let mut saw_name = false;
    loop {
        let property = c.byte()?;
        if property == 0 {
            break;
        }
        let size = c.usize_var()?;
        if size > MAX_HEADER {
            return Err(VerifyError::Invalid("file property too large"));
        }
        let raw = c.take(size)?;
        let mut p = Cursor::new(raw);
        if property == 0x0e && !saw_empty_stream {
            files.empty_stream = read_bits(&mut p, count)?;
            if !p.done() {
                return Err(VerifyError::Invalid("EmptyStream property size"));
            }
            saw_empty_stream = true;
        } else if property == 0x0f {
            if !saw_empty_stream {
                return Err(VerifyError::Invalid("EmptyFile without EmptyStream"));
            }
            let bits = read_bits(
                &mut p,
                files.empty_stream.iter().filter(|yes| **yes).count(),
            )?;
            if !p.done() {
                return Err(VerifyError::Invalid("EmptyFile property size"));
            }
            let mut j = 0;
            for (i, empty) in files.empty_stream.iter().enumerate() {
                if *empty {
                    files.empty_file[i] = bits[j];
                    j += 1;
                }
            }
        } else if property == 0x10 {
            return Err(VerifyError::Invalid("anti-items"));
        } else if property == 0x11 && !saw_name {
            if p.byte()? != 0 {
                return Err(VerifyError::Invalid("external names"));
            }
            let raw_names = p.take(p.bytes.len().saturating_sub(p.at))?;
            if raw_names.len() % 2 != 0 {
                return Err(VerifyError::Invalid("odd UTF-16 name data"));
            }
            let units = raw_names
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            let mut start = 0usize;
            let mut parsed = Vec::with_capacity(count);
            for (index, unit) in units.iter().enumerate() {
                if *unit == 0 {
                    parsed.push(
                        String::from_utf16(&units[start..index])
                            .map_err(|_| VerifyError::Invalid("invalid UTF-16 name"))?,
                    );
                    start = index + 1;
                }
            }
            if start != units.len() || parsed.len() != count {
                return Err(VerifyError::Invalid("name count or terminator"));
            }
            let total_name_bytes = parsed
                .iter()
                .try_fold(0usize, |sum, name| sum.checked_add(name.len()))
                .ok_or(VerifyError::Invalid("name budget overflow"))?;
            if total_name_bytes > MAX_NAMES {
                return Err(VerifyError::Invalid("archive name budget"));
            }
            files.names = parsed;
            saw_name = true;
        } else if property == 0x12 || property == 0x13 || property == 0x14 {
            skip_time_property(&mut p, count)?;
            if !p.done() {
                return Err(VerifyError::Invalid("time property size"));
            }
        } else if property == 0x15 {
            parse_attributes(&mut p, count, &mut files.dirs)?;
            if !p.done() {
                return Err(VerifyError::Invalid("attribute property size"));
            }
        } else if property == 0x19 {
            if raw.iter().any(|b| *b != 0) {
                return Err(VerifyError::Invalid("nonzero dummy padding"));
            }
        } else {
            return Err(VerifyError::Invalid("unknown file property"));
        }
    }
    if !saw_name && count != 0 {
        return Err(VerifyError::Invalid("missing file names"));
    }
    Ok(())
}

fn parse_attributes(c: &mut Cursor<'_>, count: usize, dirs: &mut [bool]) -> Result<()> {
    let all_defined = c.byte()?;
    let defined = match all_defined {
        0 => read_bits(c, count)?,
        1 => vec![true; count],
        _ => return Err(VerifyError::Invalid("attribute all-defined flag")),
    };
    if c.byte()? != 0 {
        return Err(VerifyError::Invalid("external attributes"));
    }
    for i in 0..count {
        if defined[i] {
            let attr = u32::from_le_bytes(
                c.take(4)?
                    .try_into()
                    .map_err(|_| VerifyError::Invalid("attribute width"))?,
            );
            if attr & 0x400 != 0 {
                return Err(VerifyError::Invalid("reparse attribute"));
            }
            // この作成プロファイルで意味を確認していない属性ビットを型判定へ流さない。
            let known = 0x0000_0001
                | 0x2
                | 0x4
                | 0x10
                | 0x20
                | 0x80
                | 0x100
                | 0x200
                | 0x1000
                | 0x2000
                | 0x4000;
            if attr & !known != 0 {
                return Err(VerifyError::Invalid("unsupported file attributes"));
            }
            dirs[i] = attr & 0x10 != 0;
        }
    }
    Ok(())
}

fn skip_time_property(c: &mut Cursor<'_>, count: usize) -> Result<()> {
    let all_defined = c.byte()?;
    let defined = match all_defined {
        0 => read_bits(c, count)?,
        1 => vec![true; count],
        _ => return Err(VerifyError::Invalid("time all-defined flag")),
    };
    if c.byte()? != 0 {
        return Err(VerifyError::Invalid("external time property"));
    }
    let values = defined
        .iter()
        .filter(|yes| **yes)
        .count()
        .checked_mul(8)
        .ok_or(VerifyError::Invalid("time size overflow"))?;
    c.take(values)?;
    Ok(())
}

fn compare_entries(manifest: &[ManifestEntry], streams: &Streams, files: &Files) -> Result<()> {
    if files.names.len() != manifest.len()
        || files.empty_stream.len() != files.names.len()
        || files.empty_file.len() != files.names.len()
        || files.dirs.len() != files.names.len()
    {
        return Err(VerifyError::Mismatch);
    }
    let mut actual: Vec<String> = Vec::with_capacity(files.names.len());
    for (i, name) in files.names.iter().enumerate() {
        let normalized = name.replace('\\', "/");
        validate_name(&normalized)?;
        if actual
            .iter()
            .any(|previous| ordinal_equal(previous, &normalized))
        {
            return Err(VerifyError::Invalid("duplicate archive name"));
        }
        let exp = manifest
            .iter()
            .find(|expected| expected.name == normalized)
            .ok_or(VerifyError::Mismatch)?;
        let is_dir = files.empty_stream[i] && !files.empty_file[i];
        if files.dirs[i] != is_dir || is_dir != (exp.kind == EntryKind::Directory) {
            return Err(VerifyError::Mismatch);
        }
        if !files.empty_stream[i] && exp.kind != EntryKind::File {
            return Err(VerifyError::Mismatch);
        }
        if files.empty_stream[i] && !is_dir {
            let empty_digest: [u8; 32] = sha2::Sha256::digest([]).into();
            if exp.kind != EntryKind::File || exp.length != 0 || exp.sha256 != empty_digest {
                return Err(VerifyError::Mismatch);
            }
        }
        actual.push(normalized);
    }
    if actual.len() != manifest.len() {
        return Err(VerifyError::Mismatch);
    }
    let nonempty = files.empty_stream.iter().filter(|empty| !**empty).count();
    if streams.folders.len() != nonempty
        || streams.packs.len() != nonempty
        || streams.pack_crcs.len() != nonempty
    {
        return Err(VerifyError::Invalid("stream/file count mismatch"));
    }
    Ok(())
}

fn verify_packed<R: Read + Seek>(
    reader: &mut R,
    header_start: u64,
    payload_total: u64,
    streams: &Streams,
    files: &Files,
    manifest: &[ManifestEntry],
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    if streams.packs.is_empty() {
        if streams.saw_pack_info
            || streams.saw_unpack_info
            || streams.saw_substreams
            || files.empty_stream.iter().any(|empty| !empty)
            || !streams.folders.is_empty()
            || header_start != 32
            || payload_total != 0
        {
            return Err(VerifyError::Invalid("missing packed data"));
        }
        return Ok(());
    }
    if !streams.saw_pack_info || !streams.saw_unpack_info {
        return Err(VerifyError::Invalid("incomplete stream sections"));
    }
    let pack_pos = streams
        .pack_pos
        .ok_or(VerifyError::Invalid("missing PackInfo"))?;
    let first = 32u64
        .checked_add(pack_pos)
        .ok_or(VerifyError::Invalid("packed offset overflow"))?;
    if first != 32 || first > header_start {
        return Err(VerifyError::Invalid("packed data must begin at offset 32"));
    }
    let mut offset = first;
    let mut stream_index = 0usize;
    let mut verified_files = HashSet::new();
    let mut decoded_total = 0u64;
    for (i, size) in streams.packs.iter().enumerate() {
        let folder = streams
            .folders
            .get(i)
            .ok_or(VerifyError::Invalid("missing folder"))?;
        let end = offset
            .checked_add(*size)
            .ok_or(VerifyError::Invalid("pack range overflow"))?;
        if end > header_start {
            return Err(VerifyError::Invalid("packed data overlaps header"));
        }
        if let Some(crc) = streams.pack_crcs[i] {
            if crc_region(reader, offset, *size, cancel, deadline)? != crc {
                return Err(VerifyError::Invalid("packed CRC"));
            }
        }
        let file_index = files
            .empty_stream
            .iter()
            .enumerate()
            .filter(|(_, empty)| !**empty)
            .nth(stream_index)
            .map(|(index, _)| index)
            .ok_or(VerifyError::Mismatch)?;
        let archive_name = files.names[file_index].replace('\\', "/");
        let manifest_file = manifest
            .iter()
            .find(|item| item.name == archive_name)
            .ok_or(VerifyError::Mismatch)?;
        decode_deflate(
            reader,
            offset,
            *size,
            folder,
            manifest_file,
            cancel,
            deadline,
        )?;
        decoded_total = decoded_total
            .checked_add(folder.unpacked)
            .ok_or(VerifyError::Invalid("decoded total overflow"))?;
        verified_files.insert(archive_name);
        offset = end;
        stream_index += 1;
    }
    let packed_total = streams
        .packs
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or(VerifyError::Invalid("packed total overflow"))?;
    if offset.checked_sub(first) != Some(packed_total)
        || offset != header_start
        || decoded_total != payload_total
    {
        return Err(VerifyError::Invalid(
            "packed region coverage or payload mismatch",
        ));
    }
    for (index, empty) in files.empty_stream.iter().enumerate() {
        if *empty && files.empty_file[index] {
            verified_files.insert(files.names[index].replace('\\', "/"));
        }
    }
    let expected_payload_files = manifest
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .count();
    if verified_files.len() != expected_payload_files {
        return Err(VerifyError::Mismatch);
    }
    Ok(())
}

fn crc_region<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    len: u64,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<u32> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|_| VerifyError::Io)?;
    let mut remaining = len;
    let mut buf = [0u8; IO_SIZE];
    let mut crc = crc32fast::Hasher::new();
    while remaining > 0 {
        check(cancel, deadline)?;
        let n = usize::try_from(remaining.min(IO_SIZE as u64))
            .map_err(|_| VerifyError::Invalid("read length overflow"))?;
        read_checked(reader, &mut buf[..n], cancel, deadline)?;
        crc.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(crc.finalize())
}

fn verify_zero_region<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    len: u64,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|_| VerifyError::Io)?;
    let mut remaining = len;
    let mut buf = [0u8; IO_SIZE];
    while remaining > 0 {
        check(cancel, deadline)?;
        let n = usize::try_from(remaining.min(IO_SIZE as u64))
            .map_err(|_| VerifyError::Invalid("padding length overflow"))?;
        read_checked(reader, &mut buf[..n], cancel, deadline)?;
        if buf[..n].iter().any(|byte| *byte != 0) {
            return Err(VerifyError::Invalid("nonzero alignment padding"));
        }
        remaining -= n as u64;
    }
    Ok(())
}

fn decode_deflate<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    packed: u64,
    folder: &Folder,
    expected: &ManifestEntry,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    if folder.unpacked != expected.length {
        return Err(VerifyError::Mismatch);
    }
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|_| VerifyError::Io)?;
    let mut decoder = Decompress::new(false);
    let mut packed_left = packed;
    let mut input = [0u8; IO_SIZE];
    let mut output = [0u8; IO_SIZE];
    let mut input_len = 0usize;
    let mut input_used = 0usize;
    let mut sha = sha2::Sha256::new();
    let mut crc = crc32fast::Hasher::new();
    let mut produced = 0u64;
    let mut ended = false;
    while !ended {
        check(cancel, deadline)?;
        if input_used == input_len && packed_left > 0 {
            input_len = usize::try_from(packed_left.min(IO_SIZE as u64))
                .map_err(|_| VerifyError::Invalid("packed chunk overflow"))?;
            read_checked(reader, &mut input[..input_len], cancel, deadline)?;
            packed_left -= input_len as u64;
            input_used = 0;
        }
        if input_used == input_len && packed_left == 0 && input_len == 0 {
            return Err(VerifyError::Invalid("truncated deflate stream"));
        }
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let flush = if packed_left == 0 && input_used == input_len {
            FlushDecompress::Finish
        } else {
            FlushDecompress::None
        };
        let status = decoder
            .decompress(&input[input_used..input_len], &mut output, flush)
            .map_err(|_| VerifyError::Invalid("invalid Deflate stream"))?;
        let consumed = usize::try_from(decoder.total_in() - before_in)
            .map_err(|_| VerifyError::Invalid("Deflate input counter"))?;
        let count = usize::try_from(decoder.total_out() - before_out)
            .map_err(|_| VerifyError::Invalid("Deflate output counter"))?;
        input_used = input_used
            .checked_add(consumed)
            .ok_or(VerifyError::Invalid("Deflate counter overflow"))?;
        produced = produced
            .checked_add(count as u64)
            .ok_or(VerifyError::Invalid("decoded size overflow"))?;
        if produced > expected.length {
            return Err(VerifyError::Mismatch);
        }
        sha.update(&output[..count]);
        crc.update(&output[..count]);
        if status == Status::StreamEnd {
            ended = true;
            if input_used != input_len || packed_left != 0 {
                return Err(VerifyError::Invalid("trailing packed bytes"));
            }
        } else if consumed == 0 && count == 0 {
            if input_used == input_len && packed_left > 0 {
                input_len = 0;
                input_used = 0;
            } else {
                return Err(VerifyError::Invalid("Deflate made no progress"));
            }
        }
    }
    if produced != expected.length || <[u8; 32]>::from(sha.finalize()) != expected.sha256 {
        return Err(VerifyError::Mismatch);
    }
    if folder.crc.is_some_and(|value| value != crc.finalize()) {
        return Err(VerifyError::Invalid("unpacked CRC"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn file(name: &str, bytes: &[u8]) -> ManifestEntry {
        ManifestEntry {
            name: name.into(),
            kind: EntryKind::File,
            length: bytes.len() as u64,
            sha256: sha2::Sha256::digest(bytes).into(),
        }
    }

    #[test]
    fn manifest_rejects_case_collisions_unsafe_names_and_missing_parent_kinds() {
        assert!(validate_manifest(&[file("a", b""), file("A", b"")]).is_err());
        for name in [
            "../x",
            "C:/x",
            "NUL.txt",
            "COM¹.txt",
            "LPT³.ext",
            "trail.",
            "bad?name",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        let entries = [file("parent", b"x"), file("parent/child", b"")];
        assert!(validate_manifest(&entries).is_err());
    }

    #[test]
    fn malformed_signature_is_rejected_without_panicking() {
        let mut bytes = Cursor::new(vec![0u8; 32]);
        assert!(
            verify_reader(
                &mut bytes,
                &[],
                Instant::now() + std::time::Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        for header in [&[0x17][..], &[1, 0x55][..], &[1, 5, 1, 0][..]] {
            assert!(parse_header(header).is_err(), "header {header:?}");
        }
        assert!(read_bits(&mut super::Cursor::new(&[0b0000_0001]), 1).is_err());
    }

    #[test]
    fn seven_zip_integer_width_boundaries_consume_each_byte_once() {
        let cases: &[(u64, &[u8])] = &[
            (127, &[0x7f]),
            (128, &[0x80, 0x80]),
            (255, &[0x80, 0xff]),
            (16_383, &[0xbf, 0xff]),
            (16_384, &[0xc0, 0x00, 0x40]),
            (2_097_151, &[0xdf, 0xff, 0xff]),
            (2_097_152, &[0xe0, 0x00, 0x00, 0x20]),
        ];
        for (expected, encoded) in cases {
            assert_eq!(varint(*expected as usize), *encoded);
            let mut bytes = encoded.to_vec();
            bytes.push(0x5a);
            let mut cursor = super::Cursor::new(bytes.as_slice());
            assert_eq!(cursor.var(), Ok(*expected));
            assert_eq!(cursor.byte(), Ok(0x5a));
            assert!(cursor.done());
        }
        assert_eq!(
            super::Cursor::new(&[0x80][..]).var(),
            Err(VerifyError::Invalid("truncated header"))
        );
    }

    #[test]
    fn timestamps_and_attributes_use_all_defined_bits_then_external_flag() {
        assert!(
            skip_time_property(
                &mut super::Cursor::new(&[1, 0, 1, 2, 3, 4, 5, 6, 7, 8][..]),
                1
            )
            .is_ok()
        );
        assert!(
            skip_time_property(
                &mut super::Cursor::new(&[0, 0x80, 0, 1, 2, 3, 4, 5, 6, 7, 8][..]),
                2
            )
            .is_ok()
        );
        assert!(
            skip_time_property(
                &mut super::Cursor::new(&[1, 1, 0, 0, 0, 0, 0, 0, 0, 0][..]),
                1
            )
            .is_err()
        );

        let mut all_dirs = [false, false];
        assert!(
            parse_attributes(
                &mut super::Cursor::new(&[1, 0, 0x10, 0, 0, 0][..]),
                1,
                &mut all_dirs[..1]
            )
            .is_ok()
        );
        assert!(all_dirs[0]);
        let mut partial_dirs = [false, false];
        assert!(
            parse_attributes(
                &mut super::Cursor::new(&[0, 0x80, 0, 0x10, 0, 0, 0][..]),
                2,
                &mut partial_dirs
            )
            .is_ok()
        );
        assert_eq!(partial_dirs, [true, false]);
        assert!(
            parse_attributes(
                &mut super::Cursor::new(&[1, 1, 0, 0, 0, 0][..]),
                1,
                &mut all_dirs[..1]
            )
            .is_err()
        );
    }

    #[test]
    fn cancellation_and_deadline_are_checked_before_io() {
        let mut bytes = Cursor::new(vec![0u8; 32]);
        assert_eq!(
            verify_reader(
                &mut bytes,
                &[],
                Instant::now() + std::time::Duration::from_secs(1),
                &AtomicBool::new(true)
            ),
            Err(VerifyError::Cancelled)
        );
        assert_eq!(
            verify_reader(&mut bytes, &[], Instant::now(), &AtomicBool::new(false)),
            Err(VerifyError::Deadline)
        );
    }

    struct CancelOnHeaderRead {
        cursor: Cursor<Vec<u8>>,
        cancelled: Arc<AtomicBool>,
    }

    impl Read for CancelOnHeaderRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let at = self.cursor.position();
            let read = self.cursor.read(buffer)?;
            if at >= 32 {
                self.cancelled.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    impl Seek for CancelOnHeaderRead {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(position)
        }
    }

    #[test]
    fn cancellation_during_final_packless_header_prevents_success() {
        let archive = empty_archive(&[("emptydir", true), ("empty.txt", false)]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut reader = CancelOnHeaderRead {
            cursor: Cursor::new(archive),
            cancelled: cancelled.clone(),
        };
        let manifest = [
            ManifestEntry {
                name: "emptydir".into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            file("empty.txt", b""),
        ];
        assert_eq!(
            verify_reader(
                &mut reader,
                &manifest,
                Instant::now() + std::time::Duration::from_secs(1),
                &cancelled
            ),
            Err(VerifyError::Cancelled)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_ordinal_collisions_do_not_expand_or_substitute_names() {
        assert!(validate_manifest(&[file("straße", b""), file("strasse", b"")]).is_ok());
        assert!(validate_manifest(&[file("Å", b""), file("å", b"")]).is_err());
        let files = Files {
            names: vec!["strasse".into()],
            empty_stream: vec![true],
            empty_file: vec![true],
            dirs: vec![false],
        };
        assert!(compare_entries(&[file("straße", b"")], &Streams::default(), &files).is_err());
        let case_files = Files {
            names: vec!["A".into()],
            ..files
        };
        assert!(compare_entries(&[file("a", b"")], &Streams::default(), &case_files).is_err());
    }

    #[test]
    fn raw_deflate_hashing_handles_multiple_bounded_chunks_and_rejects_trailing_data() {
        use flate2::{Compression, write::DeflateEncoder};
        use std::io::Write;

        let plain = (0usize..200_000)
            .map(|i| (i.wrapping_mul(37) & 0xff) as u8)
            .collect::<Vec<_>>();
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&plain).unwrap();
        let compressed = encoder.finish().unwrap();
        let entry = file("large.bin", &plain);
        let folder = Folder {
            unpacked: plain.len() as u64,
            packed: compressed.len() as u64,
            crc: Some(crc32fast::hash(&plain)),
        };
        assert_eq!(
            decode_deflate(
                &mut Cursor::new(compressed.clone()),
                0,
                compressed.len() as u64,
                &folder,
                &entry,
                &AtomicBool::new(false),
                Instant::now() + std::time::Duration::from_secs(1)
            ),
            Ok(())
        );
        let mut wrong_bytes = entry.clone();
        wrong_bytes.sha256 = sha2::Sha256::digest(b"wrong").into();
        assert_eq!(
            decode_deflate(
                &mut Cursor::new(compressed.clone()),
                0,
                compressed.len() as u64,
                &folder,
                &wrong_bytes,
                &AtomicBool::new(false),
                Instant::now() + std::time::Duration::from_secs(1)
            ),
            Err(VerifyError::Mismatch)
        );
        let mut wrong_crc = folder.clone();
        wrong_crc.crc = Some(1);
        assert!(
            decode_deflate(
                &mut Cursor::new(compressed.clone()),
                0,
                compressed.len() as u64,
                &wrong_crc,
                &entry,
                &AtomicBool::new(false),
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(
            decode_deflate(
                &mut Cursor::new(compressed[..compressed.len() - 1].to_vec()),
                0,
                (compressed.len() - 1) as u64,
                &folder,
                &entry,
                &AtomicBool::new(false),
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .is_err()
        );
        let mut trailing = compressed.clone();
        trailing.push(0);
        assert!(
            decode_deflate(
                &mut Cursor::new(trailing.clone()),
                0,
                trailing.len() as u64,
                &folder,
                &entry,
                &AtomicBool::new(false),
                Instant::now() + std::time::Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn file_table_matching_rejects_missing_extra_and_swapped_types() {
        let expected = vec![
            file("empty.txt", b""),
            ManifestEntry {
                name: "folder".into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
        ];
        let streams = Streams::default();
        let files = Files {
            names: vec!["empty.txt".into(), "folder".into()],
            empty_stream: vec![true, true],
            empty_file: vec![true, false],
            dirs: vec![false, true],
        };
        assert_eq!(compare_entries(&expected, &streams, &files), Ok(()));
        assert_eq!(
            compare_entries(&[file("empty.txt", b"")], &streams, &files),
            Err(VerifyError::Mismatch)
        );
        let extra = vec![expected[0].clone(), expected[1].clone(), file("extra", b"")];
        assert_eq!(
            compare_entries(&extra, &streams, &files),
            Err(VerifyError::Mismatch)
        );
        let swapped = Files {
            empty_file: vec![true, true],
            ..files.clone()
        };
        assert_eq!(
            compare_entries(&expected, &streams, &swapped),
            Err(VerifyError::Mismatch)
        );
    }

    fn varint(value: usize) -> Vec<u8> {
        let value = value as u64;
        for extra in 0..8usize {
            let bits = 7 + 7 * extra;
            if bits == 63 || value < (1u64 << bits) {
                let low_bits = 7 - extra;
                let prefix = if extra == 0 { 0 } else { 0xff << (8 - extra) };
                let mut encoded =
                    vec![prefix | ((value >> (8 * extra)) as u8 & ((1 << low_bits) - 1))];
                for index in 0..extra {
                    encoded.push((value >> (index * 8)) as u8);
                }
                return encoded;
            }
        }
        let mut encoded = vec![0xff];
        encoded.extend(value.to_le_bytes());
        encoded
    }

    fn empty_archive(names: &[(&str, bool)]) -> Vec<u8> {
        let count = names.len();
        let bit_bytes = count.div_ceil(8);
        let mut empty_stream = vec![0u8; bit_bytes];
        let mut empty_file = vec![0u8; bit_bytes];
        let mut attrs = vec![0u8; bit_bytes];
        let mut names_data = vec![0u8];
        let mut attrs_data = Vec::new();
        for (index, (name, is_dir)) in names.iter().enumerate() {
            empty_stream[index / 8] |= 0x80 >> (index % 8);
            if !is_dir {
                empty_file[index / 8] |= 0x80 >> (index % 8);
            }
            attrs[index / 8] |= 0x80 >> (index % 8);
            names_data.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
            names_data.extend([0, 0]);
            attrs_data.extend((if *is_dir { 0x10u32 } else { 0 }).to_le_bytes());
        }
        let mut header = vec![1, 5];
        header.extend(varint(count));
        if count > 0 {
            header.extend([0x0e]);
            header.extend(varint(bit_bytes));
            header.extend(&empty_stream);
            header.extend([0x0f]);
            header.extend(varint(bit_bytes));
            header.extend(&empty_file);
            header.extend([0x11]);
            header.extend(varint(names_data.len()));
            header.extend(&names_data);
            let mut winattrs = vec![0];
            winattrs.extend(attrs);
            winattrs.push(0);
            winattrs.extend(attrs_data);
            header.extend([0x15]);
            header.extend(varint(winattrs.len()));
            header.extend(winattrs);
        } else {
            header.extend([0x11, 1, 0]);
        }
        header.extend([0, 0]);
        let mut archive = vec![0u8; 32];
        archive[..6].copy_from_slice(SIGNATURE);
        archive[6] = 0;
        archive[7] = 4;
        archive[12..20].copy_from_slice(&0u64.to_le_bytes());
        archive[20..28].copy_from_slice(&(header.len() as u64).to_le_bytes());
        archive[28..32].copy_from_slice(&crc32fast::hash(&header).to_le_bytes());
        let start_crc = crc32fast::hash(&archive[12..32]);
        archive[8..12].copy_from_slice(&start_crc.to_le_bytes());
        archive.extend(header);
        archive
    }

    fn one_stream_archive(stream_name: &str, contents: &[u8], empty_name: &str) -> Vec<u8> {
        use flate2::{Compression, write::DeflateEncoder};
        use std::io::Write;

        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(contents).unwrap();
        let packed = encoder.finish().unwrap();
        let mut header = vec![1, 4, 6];
        header.extend([0, 1, 9]);
        header.extend(varint(packed.len()));
        header.push(0);
        header.extend([7, 0x0b, 1, 0, 1, 3, 0x04, 0x01, 0x08, 0x0c]);
        header.extend(varint(contents.len()));
        header.extend([0, 8, 0, 0, 5, 2]);
        header.extend([0x0e, 1, 0x40, 0x0f, 1, 0x80]);
        let mut names_data = vec![0];
        for name in [stream_name, empty_name] {
            names_data.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
            names_data.extend([0, 0]);
        }
        header.extend([0x11]);
        header.extend(varint(names_data.len()));
        header.extend(names_data);
        header.extend([0x15, 10, 1, 0]);
        header.extend(0u32.to_le_bytes());
        header.extend(0u32.to_le_bytes());
        header.extend([0, 0]);

        let mut archive = vec![0u8; 32];
        archive[..6].copy_from_slice(SIGNATURE);
        archive[6] = 0;
        archive[7] = 4;
        archive[12..20].copy_from_slice(&(packed.len() as u64).to_le_bytes());
        archive[20..28].copy_from_slice(&(header.len() as u64).to_le_bytes());
        archive[28..32].copy_from_slice(&crc32fast::hash(&header).to_le_bytes());
        let start_crc = crc32fast::hash(&archive[12..32]);
        archive[8..12].copy_from_slice(&start_crc.to_le_bytes());
        archive.extend(packed);
        archive.extend(header);
        archive
    }

    fn rewrite_next_header(archive: &[u8], header: &[u8]) -> Vec<u8> {
        let offset = u64::from_le_bytes(archive[12..20].try_into().unwrap());
        let start = usize::try_from(32 + offset).unwrap();
        let mut result = archive[..start].to_vec();
        result.extend(header);
        result[20..28].copy_from_slice(&(header.len() as u64).to_le_bytes());
        result[28..32].copy_from_slice(&crc32fast::hash(header).to_le_bytes());
        let start_crc = crc32fast::hash(&result[12..32]);
        result[8..12].copy_from_slice(&start_crc.to_le_bytes());
        result
    }

    fn mutate_header(archive: &[u8], mutate: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let offset = u64::from_le_bytes(archive[12..20].try_into().unwrap());
        let start = usize::try_from(32 + offset).unwrap();
        let size =
            usize::try_from(u64::from_le_bytes(archive[20..28].try_into().unwrap())).unwrap();
        let mut header = archive[start..start + size].to_vec();
        mutate(&mut header);
        rewrite_next_header(archive, &header)
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    }

    #[test]
    fn packless_archive_matches_empty_files_directories_and_special_unicode_names() {
        let names = [
            ("資料 フォルダー", true),
            ("資料 フォルダー/empty", true),
            ("empty.txt", false),
            ("-leading.txt", false),
            ("@leading.txt", false),
            ("line\u{2028}para\u{2029}.txt", false),
            ("spaces name.txt", false),
        ];
        let archive = empty_archive(&names);
        let manifest = vec![
            ManifestEntry {
                name: names[0].0.into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            ManifestEntry {
                name: names[1].0.into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            file("empty.txt", b""),
            file("-leading.txt", b""),
            file("@leading.txt", b""),
            file("line\u{2028}para\u{2029}.txt", b""),
            file("spaces name.txt", b""),
        ];
        assert_eq!(
            verify_reader(
                &mut Cursor::new(archive.clone()),
                &manifest,
                Instant::now() + std::time::Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Ok(())
        );
        assert!(
            verify_reader(
                &mut Cursor::new(archive),
                &manifest[..manifest.len() - 1],
                Instant::now() + std::time::Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_streamed_and_empty_files_are_both_verified() {
        let contents = b"captured stream bytes";
        let archive = one_stream_archive("kept.bin", contents, "claimed.bin");
        let correct = [file("kept.bin", contents), file("claimed.bin", b"")];
        assert_eq!(
            verify_reader(
                &mut Cursor::new(archive.clone()),
                &correct,
                Instant::now() + std::time::Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Ok(())
        );
        let substituted = [
            file("kept.bin", contents),
            file("claimed.bin", b"nonempty expected"),
        ];
        assert_eq!(
            verify_reader(
                &mut Cursor::new(archive),
                &substituted,
                Instant::now() + std::time::Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn full_archive_rejects_unsupported_profile_mutations_after_recomputing_crcs() {
        let expected = [file("stream.bin", b"payload"), file("empty.txt", b"")];
        let baseline = one_stream_archive("stream.bin", b"payload", "empty.txt");
        assert_eq!(
            verify_reader(
                &mut Cursor::new(baseline.clone()),
                &expected,
                Instant::now() + std::time::Duration::from_secs(2),
                &AtomicBool::new(false)
            ),
            Ok(())
        );
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "encoded header",
                mutate_header(&baseline, |header| header[0] = 0x17),
            ),
            (
                "wrong codec",
                mutate_header(&baseline, |header| {
                    let at = find_bytes(header, &[4, 1, 8]);
                    header[at + 2] = 7;
                }),
            ),
            (
                "complex coder",
                mutate_header(&baseline, |header| {
                    let at = find_bytes(header, &[1, 3, 4, 1, 8]);
                    header[at + 1] = 0x13;
                }),
            ),
            (
                "external folder",
                mutate_header(&baseline, |header| {
                    let at = find_bytes(header, &[7, 0x0b, 1, 0]);
                    header[at + 3] = 1;
                }),
            ),
            (
                "external names",
                mutate_header(&baseline, |header| {
                    let at = find_bytes(header, &[0x11]);
                    let name_at = at + 2;
                    header[name_at] = 1;
                }),
            ),
            (
                "solid folder",
                mutate_header(&baseline, |header| {
                    let at = find_bytes(header, &[8, 0, 0, 5, 2]);
                    header.splice(at + 1..at + 2, [0x0d, 2, 0]);
                }),
            ),
            (
                "reparse attribute",
                mutate_header(&baseline, |header| {
                    let at = find_bytes(header, &[0x15, 10, 1, 0]);
                    header[at + 4] = 0;
                    header[at + 5] = 4;
                }),
            ),
            (
                "anti item",
                mutate_header(&baseline, |header| {
                    let at = header.len() - 2;
                    header.splice(at..at, [0x10, 1, 0x80]);
                }),
            ),
        ];
        for (name, candidate) in cases {
            let reason = match name {
                "encoded header" => "encoded or non-header next header",
                "wrong codec" => "not raw Deflate",
                "complex coder" => "complex coder",
                "external folder" => "external folders",
                "external names" => "external names",
                "solid folder" => "multiple substreams per folder",
                "reparse attribute" => "reparse attribute",
                "anti item" => "anti-items",
                _ => unreachable!(),
            };
            assert_eq!(
                verify_reader(
                    &mut Cursor::new(candidate),
                    &expected,
                    Instant::now() + std::time::Duration::from_secs(2),
                    &AtomicBool::new(false)
                ),
                Err(VerifyError::Invalid(reason)),
                "wrong rejection boundary for {name}"
            );
        }
    }

    #[test]
    fn full_archive_enforces_size_names_and_packed_region_budgets() {
        let expected = [file("stream.bin", b"payload"), file("empty.txt", b"")];
        let baseline = one_stream_archive("stream.bin", b"payload", "empty.txt");
        let gap = mutate_header(&baseline, |header| {
            let at = find_bytes(header, &[6, 0, 1, 9]);
            header[at + 1] = 1;
        });
        let mut trailing_region = baseline.clone();
        let header_at =
            32 + u64::from_le_bytes(trailing_region[12..20].try_into().unwrap()) as usize;
        trailing_region.insert(header_at, 0);
        let offset = u64::from_le_bytes(trailing_region[12..20].try_into().unwrap());
        trailing_region[12..20].copy_from_slice(&(offset + 1).to_le_bytes());
        let start_crc = crc32fast::hash(&trailing_region[12..32]);
        trailing_region[8..12].copy_from_slice(&start_crc.to_le_bytes());
        let cases = [
            ("packed prefix gap", gap),
            ("trailing packed region", trailing_region),
        ];
        for (name, candidate) in cases {
            assert!(
                verify_reader(
                    &mut Cursor::new(candidate),
                    &expected,
                    Instant::now() + std::time::Duration::from_secs(2),
                    &AtomicBool::new(false)
                )
                .is_err(),
                "accepted {name}"
            );
        }
        let oversized_manifest = vec![file("x", b""); MAX_ENTRIES + 1];
        assert!(validate_manifest(&oversized_manifest).is_err());
        assert!(parse_header(&vec![1; MAX_HEADER + 1]).is_err());
        let mut over_count_header = vec![1, 5];
        over_count_header.extend(varint(MAX_ENTRIES + 1));
        over_count_header.extend([0, 0]);
        assert!(parse_header(&over_count_header).is_err());
        let overflow_manifest = [
            ManifestEntry {
                name: "a".into(),
                kind: EntryKind::File,
                length: u64::MAX,
                sha256: [1; 32],
            },
            ManifestEntry {
                name: "b".into(),
                kind: EntryKind::File,
                length: 1,
                sha256: [1; 32],
            },
        ];
        assert!(validate_manifest(&overflow_manifest).is_err());
        let mut wrong_bytes = expected.clone();
        wrong_bytes[0] = file("stream.bin", b"payloae");
        assert!(
            verify_reader(
                &mut Cursor::new(baseline.clone()),
                &wrong_bytes,
                Instant::now() + std::time::Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        let mut wrong_length = expected;
        wrong_length[0].length += 1;
        assert!(
            verify_reader(
                &mut Cursor::new(baseline),
                &wrong_length,
                Instant::now() + std::time::Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn archive_crc_truncation_and_checked_offset_fail_closed() {
        let empty = empty_archive(&[]);
        let mut bad_start_crc = empty.clone();
        bad_start_crc[8] ^= 1;
        let mut bad_header_crc = empty.clone();
        bad_header_crc[28] ^= 1;
        let start_crc = crc32fast::hash(&bad_header_crc[12..32]);
        bad_header_crc[8..12].copy_from_slice(&start_crc.to_le_bytes());
        let mut truncated = empty.clone();
        truncated.pop();
        let mut overflow_offset = empty;
        overflow_offset[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        let start_crc = crc32fast::hash(&overflow_offset[12..32]);
        overflow_offset[8..12].copy_from_slice(&start_crc.to_le_bytes());
        for (candidate, reason) in [
            (bad_start_crc, "start header CRC"),
            (bad_header_crc, "next header CRC"),
            (truncated, "header range or size"),
            (overflow_offset, "offset overflow"),
        ] {
            assert_eq!(
                verify_reader(
                    &mut Cursor::new(candidate),
                    &[],
                    Instant::now() + std::time::Duration::from_secs(1),
                    &AtomicBool::new(false)
                ),
                Err(VerifyError::Invalid(reason))
            );
        }
    }

    #[test]
    #[ignore = "requires a fixed-profile 7z archive produced by the primary integration fixture"]
    fn verifies_real_profile_archive() {
        use std::path::PathBuf;
        let path = std::env::var_os("GPUI_FILER_7ZIP_VERIFY_ARCHIVE")
            .expect("set GPUI_FILER_7ZIP_VERIFY_ARCHIVE to the real test archive");
        let mut archive_file = File::open(PathBuf::from(path)).expect("open verifier test archive");
        let manifest = vec![
            ManifestEntry {
                name: "資料 フォルダー".into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            ManifestEntry {
                name: "資料 フォルダー/入れ子".into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            ManifestEntry {
                name: "資料 フォルダー/empty".into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            file("資料 フォルダー/入れ子/data.bin", &[0, 1, 2, 0, 255, 127]),
            file("-leading.txt", b"dash\n"),
            file("@leading.txt", b"at\n"),
            file("empty.txt", b""),
            file("line\u{2028}para\u{2029}.txt", b"unicode separators\n"),
            file("spaces name.txt", b"space\n"),
        ];
        verify(
            &mut archive_file,
            &manifest,
            Instant::now() + std::time::Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    }

    #[test]
    #[ignore = "requires a fixed-profile real 7z packless archive"]
    fn verifies_real_empty_profile_archive() {
        use std::path::PathBuf;
        let path = std::env::var_os("GPUI_FILER_7ZIP_VERIFY_EMPTY_ARCHIVE")
            .expect("set GPUI_FILER_7ZIP_VERIFY_EMPTY_ARCHIVE to the real packless archive");
        let mut archive_file =
            File::open(PathBuf::from(path)).expect("open packless verifier archive");
        let manifest = [
            ManifestEntry {
                name: "emptydir".into(),
                kind: EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            file("empty.txt", b""),
        ];
        verify(
            &mut archive_file,
            &manifest,
            Instant::now() + std::time::Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    }
}
