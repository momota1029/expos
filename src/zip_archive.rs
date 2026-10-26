use std::{
    cmp::Ordering,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

use zip::ZipArchive;

#[path = "zip_archive/validated_reader.rs"]
#[cfg(test)]
pub(crate) mod validated_reader;
#[path = "zip_archive/validated_reader.rs"]
#[cfg(not(test))]
mod validated_reader;

macro_rules! central_entry_type {
    ($vis:vis) => {
        #[derive(Clone, Debug)]
        $vis struct CentralEntry {
            $vis name: Vec<u8>,
            $vis flags: u16,
            $vis method: u16,
            $vis crc32: u32,
            $vis compressed: u64,
            $vis uncompressed: u64,
            $vis local_offset: u64,
            $vis data_start: u64,
            $vis external: u32,
            $vis made_by: u8,
        }
    };
}
macro_rules! scanned_archive_type {
    ($vis:vis) => {
        #[derive(Clone, Debug)]
        $vis struct ScannedArchive {
            $vis entries: Vec<CentralEntry>,
            $vis layout: validated_reader::Layout,
            $vis cd_start: u64,
        }
    };
}
#[cfg(test)]
central_entry_type!(pub(crate));
#[cfg(not(test))]
central_entry_type!(pub(self));
#[cfg(test)]
scanned_archive_type!(pub(crate));
#[cfg(not(test))]
scanned_archive_type!(pub(self));

const MAX_CENTRAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAX_DEPTH: usize = 128;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_ZIP64_EXTENSION: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestNode {
    pub(crate) components: Vec<String>,
    pub(crate) kind: EntryKind,
    pub(crate) explicit: bool,
    pub(crate) entry_index: Option<usize>,
    pub(crate) declared_uncompressed_size: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ZipReadError(pub(crate) String);

impl std::fmt::Display for ZipReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for ZipReadError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

fn scan_central_directory_impl(
    file: &mut File,
    cancelled: &AtomicBool,
) -> Result<ScannedArchive, ZipReadError> {
    let len = file
        .metadata()
        .map_err(|e| ZipReadError(e.to_string()))?
        .len();
    let tail_len = len.min(65_557) as usize;
    let mut tail = vec![0; tail_len];
    file.seek(SeekFrom::End(-(tail_len as i64)))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|e| ZipReadError(e.to_string()))?;
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            tail.get(i..i + 4) == Some(b"PK\x05\x06".as_slice())
                && i + 22 + u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize == tail.len()
        })
        .ok_or_else(|| ZipReadError("ZIP end record not found".into()))?;
    let absolute_eocd = len - tail_len as u64 + eocd as u64;
    let eocd_start = absolute_eocd;
    let mut approved = vec![validated_reader::ReadSegment {
        start: eocd_start,
        end: eocd_start + 22,
    }];
    let mut seek_points = vec![validated_reader::SeekPoint {
        offset: eocd_start,
        first_read_cap: 22,
    }];
    let mut eocd_header = [0u8; 22];
    eocd_header.copy_from_slice(&tail[eocd..eocd + 22]);
    eocd_header[20..22].fill(0);
    let classic_disk = u16::from_le_bytes([tail[eocd + 4], tail[eocd + 5]]);
    let classic_cd_disk = u16::from_le_bytes([tail[eocd + 6], tail[eocd + 7]]);
    let classic_on_disk = u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]);
    let classic_total = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let classic_cd_size = u32::from_le_bytes(tail[eocd + 12..eocd + 16].try_into().unwrap());
    let classic_cd_offset = u32::from_le_bytes(tail[eocd + 16..eocd + 20].try_into().unwrap());
    let has_locator = absolute_eocd >= 20 && {
        file.seek(SeekFrom::Start(absolute_eocd - 20))
            .and_then(|_| {
                let mut sig = [0; 4];
                file.read_exact(&mut sig)?;
                Ok(sig == *b"PK\x06\x07")
            })
            .map_err(|e| ZipReadError(e.to_string()))?
    };
    let locator_start = has_locator.then(|| absolute_eocd - 20);
    if let Some(start) = locator_start {
        approved.push(validated_reader::ReadSegment {
            start,
            end: start + 20,
        });
        seek_points.push(validated_reader::SeekPoint {
            offset: start,
            first_read_cap: 20,
        });
    }
    let (disk, cd_disk, on_disk, total, cd_size, cd_offset, cd_end) = if has_locator {
        let mut locator = [0u8; 20];
        file.seek(SeekFrom::Start(absolute_eocd - 20))
            .and_then(|_| file.read_exact(&mut locator))
            .map_err(|e| ZipReadError(e.to_string()))?;
        let locator_disk = u32::from_le_bytes(locator[4..8].try_into().unwrap());
        let zip64_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap());
        let disks = u32::from_le_bytes(locator[16..20].try_into().unwrap());
        if locator_disk != 0 || disks != 1 {
            return Err(ZipReadError(
                "multi-disk ZIP64 archives are unsupported".into(),
            ));
        }
        let mut footer = [0u8; 56];
        file.seek(SeekFrom::Start(zip64_offset))
            .and_then(|_| file.read_exact(&mut footer))
            .map_err(|e| ZipReadError(e.to_string()))?;
        if &footer[..4] != b"PK\x06\x06" {
            return Err(ZipReadError("invalid ZIP64 footer signature".into()));
        }
        let record_size = u64::from_le_bytes(footer[4..12].try_into().unwrap());
        let extension = record_size
            .checked_sub(44)
            .ok_or_else(|| ZipReadError("invalid ZIP64 footer size".into()))?;
        if extension > MAX_ZIP64_EXTENSION
            || zip64_offset
                .checked_add(12)
                .and_then(|v| v.checked_add(record_size))
                != Some(absolute_eocd - 20)
            || u16::from_le_bytes([footer[14], footer[15]]) > 45
        {
            return Err(ZipReadError(
                "invalid or unsupported ZIP64 footer extent/version".into(),
            ));
        }
        approved.push(validated_reader::ReadSegment {
            start: zip64_offset,
            end: zip64_offset + 56,
        });
        seek_points.push(validated_reader::SeekPoint {
            offset: zip64_offset,
            first_read_cap: 56,
        });
        if extension > 0 {
            approved.push(validated_reader::ReadSegment {
                start: zip64_offset + 56,
                end: zip64_offset + 56 + extension,
            });
        }
        let disk = u32::from_le_bytes(footer[16..20].try_into().unwrap());
        let cd_disk = u32::from_le_bytes(footer[20..24].try_into().unwrap());
        let on_disk = u64::from_le_bytes(footer[24..32].try_into().unwrap());
        let total = u64::from_le_bytes(footer[32..40].try_into().unwrap());
        let cd_size = u64::from_le_bytes(footer[40..48].try_into().unwrap());
        let cd_offset = u64::from_le_bytes(footer[48..56].try_into().unwrap());
        if (classic_disk != u16::MAX && u32::from(classic_disk) != disk)
            || (classic_cd_disk != u16::MAX && u32::from(classic_cd_disk) != cd_disk)
            || (classic_on_disk != u16::MAX && u64::from(classic_on_disk) != on_disk)
            || (classic_total != u16::MAX && u64::from(classic_total) != total)
            || (classic_cd_size != u32::MAX && u64::from(classic_cd_size) != cd_size)
            || (classic_cd_offset != u32::MAX && u64::from(classic_cd_offset) != cd_offset)
        {
            return Err(ZipReadError(
                "ZIP64 and classic footer fields disagree".into(),
            ));
        }
        (
            disk,
            cd_disk,
            on_disk,
            total,
            cd_size,
            cd_offset,
            zip64_offset,
        )
    } else {
        if classic_on_disk == u16::MAX
            || classic_total == u16::MAX
            || classic_cd_size == u32::MAX
            || classic_cd_offset == u32::MAX
        {
            return Err(ZipReadError("ZIP64 sentinel without locator".into()));
        }
        let cd_end = absolute_eocd;
        (
            u32::from(classic_disk),
            u32::from(classic_cd_disk),
            u64::from(classic_on_disk),
            u64::from(classic_total),
            u64::from(classic_cd_size),
            u64::from(classic_cd_offset),
            cd_end,
        )
    };
    if disk != 0 || cd_disk != 0 || on_disk != total {
        return Err(ZipReadError(
            "multi-disk ZIP archives are unsupported".into(),
        ));
    }
    let total =
        usize::try_from(total).map_err(|_| ZipReadError("ZIP entry count overflow".into()))?;
    if total > MAX_ENTRIES || cd_size > MAX_CENTRAL_BYTES {
        return Err(ZipReadError(
            "ZIP central directory exceeds the configured bounds".into(),
        ));
    }
    let cd_start = cd_end
        .checked_sub(cd_size)
        .ok_or_else(|| ZipReadError("invalid central-directory range".into()))?;
    if cd_start != cd_offset {
        return Err(ZipReadError(
            "self-extracting/prefixed ZIP archives are unsupported".into(),
        ));
    }
    if has_locator {
        // ZIP64の有効な値をLibraryにも選ばせ、Classic 0値を空ZIPとして先に解釈させない。
        eocd_header[8..12].copy_from_slice(&u16::MAX.to_le_bytes().repeat(2));
        eocd_header[12..20].copy_from_slice(&u32::MAX.to_le_bytes().repeat(2));
    }
    file.seek(SeekFrom::Start(cd_start))
        .map_err(|e| ZipReadError(e.to_string()))?;
    let mut entries = Vec::with_capacity(total);
    let mut consumed = 0u64;
    for _ in 0..total {
        if cancelled.load(AtomicOrdering::Acquire) {
            return Err(ZipReadError("cancelled".into()));
        }
        let record_start = file
            .stream_position()
            .map_err(|e| ZipReadError(e.to_string()))?;
        approved.push(validated_reader::ReadSegment {
            start: record_start,
            end: record_start + 46,
        });
        seek_points.push(validated_reader::SeekPoint {
            offset: record_start,
            first_read_cap: 46,
        });
        let mut fixed = [0; 46];
        file.read_exact(&mut fixed)
            .map_err(|e| ZipReadError(e.to_string()))?;
        if &fixed[..4] != b"PK\x01\x02" {
            return Err(ZipReadError("invalid central-directory record".into()));
        }
        let flags = u16::from_le_bytes([fixed[8], fixed[9]]);
        let method = u16::from_le_bytes([fixed[10], fixed[11]]);
        let compressed32 = u32::from_le_bytes(fixed[20..24].try_into().unwrap());
        let uncompressed32 = u32::from_le_bytes(fixed[24..28].try_into().unwrap());
        let crc32 = u32::from_le_bytes(fixed[16..20].try_into().unwrap());
        let local_offset32 = u32::from_le_bytes(fixed[42..46].try_into().unwrap());
        let name_len = u16::from_le_bytes([fixed[28], fixed[29]]) as usize;
        let extra_len = u16::from_le_bytes([fixed[30], fixed[31]]) as usize;
        let comment_len = u16::from_le_bytes([fixed[32], fixed[33]]) as usize;
        let disk_start16 = u16::from_le_bytes([fixed[34], fixed[35]]);
        let external = u32::from_le_bytes(fixed[38..42].try_into().unwrap());
        let made_by = fixed[5];
        let record_bytes = 46usize
            .checked_add(name_len)
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(comment_len))
            .ok_or_else(|| ZipReadError("central record length overflow".into()))?;
        consumed = consumed
            .checked_add(record_bytes as u64)
            .ok_or_else(|| ZipReadError("central directory overflow".into()))?;
        if consumed > cd_size || consumed > MAX_CENTRAL_BYTES {
            return Err(ZipReadError(
                "central directory exceeds its declared bound".into(),
            ));
        }
        let mut name = vec![0; name_len];
        let name_start = file
            .stream_position()
            .map_err(|e| ZipReadError(e.to_string()))?;
        if name_len > 0 {
            approved.push(validated_reader::ReadSegment {
                start: name_start,
                end: name_start + name_len as u64,
            });
        }
        file.read_exact(&mut name)
            .map_err(|e| ZipReadError(e.to_string()))?;
        let extra_start = file
            .stream_position()
            .map_err(|e| ZipReadError(e.to_string()))?;
        if extra_len > 0 {
            approved.push(validated_reader::ReadSegment {
                start: extra_start,
                end: extra_start + extra_len as u64,
            });
        }
        let mut extra = vec![0; extra_len];
        file.read_exact(&mut extra)
            .map_err(|e| ZipReadError(e.to_string()))?;
        let comment_start = file
            .stream_position()
            .map_err(|e| ZipReadError(e.to_string()))?;
        if comment_len > 0 {
            approved.push(validated_reader::ReadSegment {
                start: comment_start,
                end: comment_start + comment_len as u64,
            });
        }
        file.seek(SeekFrom::Current(comment_len as i64))
            .map_err(|e| ZipReadError(e.to_string()))?;
        if flags & 1 != 0 {
            return Err(ZipReadError("encrypted ZIP entries are unsupported".into()));
        }
        if flags & (1 << 11) != 0 && std::str::from_utf8(&name).is_err() {
            return Err(ZipReadError("ZIP entry name has invalid UTF-8".into()));
        }
        let (uncompressed, compressed, local_offset, disk_start) = resolve_zip64_extra_impl(
            &extra,
            uncompressed32 == u32::MAX,
            compressed32 == u32::MAX,
            local_offset32 == u32::MAX,
            disk_start16 == u16::MAX,
            uncompressed32 as u64,
            compressed32 as u64,
            local_offset32 as u64,
            disk_start16 as u32,
        )?;
        if disk_start != 0 {
            return Err(ZipReadError("multi-disk ZIP entry is unsupported".into()));
        }
        if method != 0 && method != 8 {
            return Err(ZipReadError("unsupported ZIP compression method".into()));
        }
        if external & (0x0400 | 0x0048) != 0 {
            return Err(ZipReadError(
                "reparse/special ZIP entry attributes are unsupported".into(),
            ));
        }
        if made_by == 3 {
            let mode = external >> 16;
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(ZipReadError(
                    "link or special ZIP entry attributes are unsupported".into(),
                ));
            }
        }
        let record_end = file
            .stream_position()
            .map_err(|e| ZipReadError(e.to_string()))?;
        if local_offset >= cd_start {
            return Err(ZipReadError("invalid local header offset".into()));
        }
        approved.push(validated_reader::ReadSegment {
            start: local_offset,
            end: local_offset + 30,
        });
        seek_points.push(validated_reader::SeekPoint {
            offset: local_offset,
            first_read_cap: 30,
        });
        file.seek(SeekFrom::Start(local_offset))
            .map_err(|e| ZipReadError(e.to_string()))?;
        let mut local = [0u8; 30];
        file.read_exact(&mut local)
            .map_err(|e| ZipReadError(e.to_string()))?;
        if &local[..4] != b"PK\x03\x04" {
            return Err(ZipReadError("invalid local file header".into()));
        }
        let local_name_len = u16::from_le_bytes([local[26], local[27]]) as u64;
        let local_extra_len = u16::from_le_bytes([local[28], local[29]]) as u64;
        let data_start = local_offset
            .checked_add(30)
            .and_then(|v| v.checked_add(local_name_len))
            .and_then(|v| v.checked_add(local_extra_len))
            .ok_or_else(|| ZipReadError("local header range overflow".into()))?;
        let data_end = data_start
            .checked_add(compressed)
            .ok_or_else(|| ZipReadError("compressed data range overflow".into()))?;
        if data_end > cd_start {
            return Err(ZipReadError(
                "compressed data overlaps the central directory".into(),
            ));
        }
        file.seek(SeekFrom::Start(record_end))
            .map_err(|e| ZipReadError(e.to_string()))?;
        entries.push(CentralEntry {
            name,
            flags,
            method,
            crc32,
            compressed,
            uncompressed,
            local_offset,
            data_start,
            external,
            made_by,
        });
    }
    if consumed != cd_size {
        return Err(ZipReadError("central-directory byte count mismatch".into()));
    }
    if total > 0 && !entries.iter().any(|entry| entry.local_offset == 0) {
        return Err(ZipReadError("prefixed ZIP archives are unsupported".into()));
    }
    approved.sort_by_key(|segment| segment.start);
    seek_points.sort_by_key(|point| point.offset);
    Ok(ScannedArchive {
        entries,
        cd_start,
        layout: validated_reader::Layout {
            logical_eof: eocd_start + 22,
            eocd_start,
            eocd: eocd_header,
            segments: approved,
            seek_points,
        },
    })
}

#[cfg(test)]
pub(crate) fn scan_central_directory(
    file: &mut File,
    cancelled: &AtomicBool,
) -> Result<ScannedArchive, ZipReadError> {
    scan_central_directory_impl(file, cancelled)
}

fn resolve_zip64_extra_impl(
    extra: &[u8],
    need_uncompressed: bool,
    need_compressed: bool,
    need_offset: bool,
    need_disk: bool,
    classic_uncompressed: u64,
    classic_compressed: u64,
    classic_offset: u64,
    classic_disk: u32,
) -> Result<(u64, u64, u64, u32), ZipReadError> {
    let mut pos = 0usize;
    let mut payload = None;
    while pos + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[pos], extra[pos + 1]]);
        let len = u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]) as usize;
        let data_start = pos + 4;
        let Some(next) = data_start.checked_add(len) else {
            return Err(ZipReadError("ZIP extra-field length overflow".into()));
        };
        if next > extra.len() {
            return Err(ZipReadError("truncated ZIP extra field".into()));
        }
        if id == 1 {
            if payload.replace(&extra[data_start..next]).is_some() {
                return Err(ZipReadError("duplicate ZIP64 extra field".into()));
            }
        }
        pos = next;
    }
    if pos != extra.len() {
        return Err(ZipReadError("malformed ZIP extra-field framing".into()));
    }
    let required = usize::from(need_uncompressed) * 8
        + usize::from(need_compressed) * 8
        + usize::from(need_offset) * 8
        + usize::from(need_disk) * 4;
    if required == 0 {
        if payload.is_some() {
            return Err(ZipReadError("surplus ZIP64 extra-field payload".into()));
        }
        return Ok((
            classic_uncompressed,
            classic_compressed,
            classic_offset,
            classic_disk,
        ));
    }
    let payload = payload.ok_or_else(|| ZipReadError("missing ZIP64 extra field".into()))?;
    if payload.len() != required {
        return Err(ZipReadError("invalid ZIP64 extra-field length".into()));
    }
    let mut cursor = 0;
    let mut read64 = || {
        let value = u64::from_le_bytes(payload[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;
        value
    };
    let uncompressed = if need_uncompressed {
        read64()
    } else {
        classic_uncompressed
    };
    let compressed = if need_compressed {
        read64()
    } else {
        classic_compressed
    };
    let local_offset = if need_offset {
        read64()
    } else {
        classic_offset
    };
    let disk = if need_disk {
        u32::from_le_bytes(payload[cursor..cursor + 4].try_into().unwrap())
    } else {
        classic_disk
    };
    Ok((uncompressed, compressed, local_offset, disk))
}

#[cfg(test)]
pub(crate) fn resolve_zip64_extra(
    extra: &[u8],
    need_uncompressed: bool,
    need_compressed: bool,
    need_offset: bool,
    need_disk: bool,
    base_uncompressed: u64,
    base_compressed: u64,
    base_offset: u64,
    base_disk: u32,
) -> Result<(u64, u64, u64, u32), ZipReadError> {
    resolve_zip64_extra_impl(
        extra,
        need_uncompressed,
        need_compressed,
        need_offset,
        need_disk,
        base_uncompressed,
        base_compressed,
        base_offset,
        base_disk,
    )
}

fn build_manifest_from_archive_impl<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    central: &[CentralEntry],
) -> Result<Vec<ManifestNode>, ZipReadError> {
    if central.len() > MAX_ENTRIES {
        return Err(ZipReadError(
            "ZIP entry count exceeds the configured bound".into(),
        ));
    }
    let mut nodes = Vec::<ManifestNode>::new();
    let mut normalized_bytes = 0usize;
    for index in 0..central.len() {
        let meta = &central[index];
        if meta.flags & 1 != 0 || (meta.method != 0 && meta.method != 8) {
            return Err(ZipReadError("encrypted or unsupported ZIP entry".into()));
        }
        let entry = archive
            .by_index(index)
            .map_err(|e| ZipReadError(format!("ZIP entry parse failed: {e}")))?;
        let parser_method = match entry.compression() {
            zip::CompressionMethod::Stored => 0,
            zip::CompressionMethod::Deflated => 8,
            _ => u16::MAX,
        };
        if entry.name_raw() != meta.name
            || entry.size() != meta.uncompressed
            || entry.compressed_size() != meta.compressed
            || entry.crc32() != meta.crc32
            || parser_method != meta.method
            || entry.header_start() != meta.local_offset
            || entry.data_start() != meta.data_start
        {
            return Err(ZipReadError(
                "ZIP parser entry metadata disagrees with the bounded scan".into(),
            ));
        }
        let name = entry.name();
        if name.is_empty() || name.starts_with('/') || name.contains('\\') {
            return Err(ZipReadError("absolute or malformed ZIP entry path".into()));
        }
        let directory_marker = name.ends_with('/');
        let clean = if directory_marker {
            &name[..name.len() - 1]
        } else {
            name
        };
        if clean.is_empty() || clean.ends_with('/') {
            return Err(ZipReadError("empty ZIP path component".into()));
        }
        let components = clean.split('/').map(str::to_owned).collect::<Vec<_>>();
        if components.len() > MAX_DEPTH {
            return Err(ZipReadError("ZIP path exceeds maximum depth".into()));
        }
        if components
            .iter()
            .any(|part| !crate::windows_name::is_single_name(part))
        {
            return Err(ZipReadError(
                "ZIP path contains an unsafe Windows component".into(),
            ));
        }
        let is_dir = directory_marker || entry.is_dir();
        let dos_directory = meta.external & 0x10 != 0;
        let unix_kind = if meta.made_by == 3 {
            (meta.external >> 16) & 0o170000
        } else {
            0
        };
        if (dos_directory && !directory_marker)
            || (unix_kind == 0o100000 && is_dir)
            || (unix_kind == 0o040000 && !is_dir)
        {
            return Err(ZipReadError(
                "ZIP file type attributes disagree with the entry path".into(),
            ));
        }
        if is_dir && entry.size() != 0 {
            return Err(ZipReadError("directory entry has data".into()));
        }
        let kind = if is_dir {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        for prefix_len in 1..=components.len() {
            let prefix = components[..prefix_len].to_vec();
            normalized_bytes = normalized_bytes
                .checked_add(
                    prefix.iter().map(|part| part.len()).sum::<usize>()
                        + prefix_len.saturating_sub(1),
                )
                .filter(|total| *total <= MAX_NAME_BYTES)
                .ok_or_else(|| {
                    ZipReadError("normalized ZIP names exceed the 8 MiB bound".into())
                })?;
            if nodes.len() == MAX_ENTRIES {
                return Err(ZipReadError(
                    "materialized ZIP tree exceeds the entry bound".into(),
                ));
            }
            let leaf = prefix_len == components.len();
            let node_kind = if !leaf || is_dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            nodes.push(ManifestNode {
                components: prefix,
                kind: node_kind,
                explicit: leaf,
                entry_index: (leaf && kind == EntryKind::File).then_some(index),
                declared_uncompressed_size: (leaf && kind == EntryKind::File)
                    .then_some(entry.size()),
            });
        }
    }
    if nodes.len() > MAX_ENTRIES {
        return Err(ZipReadError(
            "materialized ZIP tree exceeds the entry bound".into(),
        ));
    }
    nodes.sort_by(|a, b| compare_paths_impl(&a.components, &b.components));
    let mut folded = Vec::<ManifestNode>::with_capacity(nodes.len());
    for node in nodes {
        if let Some(previous) = folded
            .last_mut()
            .filter(|p| compare_paths_impl(&p.components, &node.components) == Ordering::Equal)
        {
            if previous.components != node.components
                || previous.kind != node.kind
                || (previous.explicit && node.explicit)
            {
                return Err(ZipReadError("duplicate or case-colliding ZIP paths".into()));
            }
            if node.explicit {
                previous.explicit = true;
                previous.entry_index = node.entry_index;
            }
        } else {
            folded.push(node);
        }
    }
    let mut name_bytes = 0usize;
    for node in &folded {
        name_bytes = name_bytes
            .checked_add(
                node.components.iter().map(|p| p.len()).sum::<usize>()
                    + node.components.len().saturating_sub(1),
            )
            .ok_or_else(|| {
                ZipReadError("normalized ZIP names exceed the configured bound".into())
            })?;
        if name_bytes > MAX_NAME_BYTES {
            return Err(ZipReadError(
                "normalized ZIP names exceed the 8 MiB bound".into(),
            ));
        }
        for depth in 1..node.components.len() {
            let prefix = &node.components[..depth];
            let found = folded
                .binary_search_by(|candidate| compare_paths_impl(&candidate.components, prefix));
            if found
                .ok()
                .is_none_or(|i| folded[i].kind != EntryKind::Directory)
            {
                return Err(ZipReadError("file/directory path conflict".into()));
            }
        }
    }
    if folded.len() > MAX_ENTRIES {
        return Err(ZipReadError(
            "materialized ZIP tree exceeds the entry bound".into(),
        ));
    }
    Ok(folded)
}

#[cfg(test)]
pub(crate) fn build_manifest_from_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    central: &[CentralEntry],
) -> Result<Vec<ManifestNode>, ZipReadError> {
    build_manifest_from_archive_impl(archive, central)
}

fn compare_paths_impl(a: &[String], b: &[String]) -> Ordering {
    for (left, right) in a.iter().zip(b) {
        let ordering = compare_component(left, right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
pub(crate) fn compare_paths(a: &[String], b: &[String]) -> Ordering {
    compare_paths_impl(a, b)
}

fn compare_component(a: &str, b: &str) -> Ordering {
    #[cfg(windows)]
    {
        let aw = a.encode_utf16().collect::<Vec<_>>();
        let bw = b.encode_utf16().collect::<Vec<_>>();
        let result = unsafe {
            windows::Win32::Globalization::CompareStringOrdinal(
                &aw,
                &bw,
                windows::Win32::Foundation::BOOL(1),
            )
        };
        match result.0 {
            1 => Ordering::Less,
            2 => Ordering::Equal,
            3 => Ordering::Greater,
            _ => a.cmp(b),
        }
    }
    #[cfg(not(windows))]
    {
        a.to_lowercase().cmp(&b.to_lowercase())
    }
}
// metadata 検証の境界を閉じ、通常読込は呼出側へ一時的に貸す。戻り値は reader 等を借用できない。
pub(crate) fn with_validated_archive<T, E>(
    file: &mut File,
    cancelled: &AtomicBool,
    callback: impl for<'a> FnOnce(
        &'a mut ZipArchive<validated_reader::Reader<'a, File>>,
        &'a [ManifestNode],
    ) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<ZipReadError>,
{
    let scanned = scan_central_directory_impl(file, cancelled).map_err(E::from)?;
    let mode = std::cell::Cell::new(false);
    let reader = validated_reader::Reader::new(
        file.try_clone()
            .map_err(|e| E::from(ZipReadError(e.to_string())))?,
        scanned.layout.clone(),
        &mode,
    );
    let mut archive = ZipArchive::with_config(
        zip::read::Config {
            archive_offset: zip::read::ArchiveOffset::Known(0),
        },
        reader,
    )
    .map_err(|e| E::from(ZipReadError(format!("invalid or unsupported ZIP: {e}"))))?;
    if archive.offset() != 0 || archive.central_directory_start() != scanned.cd_start {
        return Err(E::from(ZipReadError(
            "ZIP parser metadata disagrees with the validated layout".into(),
        )));
    }
    if archive.len() != scanned.entries.len() {
        return Err(E::from(ZipReadError(
            "central-directory count changed".into(),
        )));
    }
    mode.set(true);
    let manifest =
        build_manifest_from_archive_impl(&mut archive, &scanned.entries).map_err(E::from)?;
    callback(&mut archive, &manifest)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveManifest {
    pub(crate) nodes: Vec<ManifestNode>,
}

pub(crate) fn inspect(
    file: &mut File,
    cancelled: &AtomicBool,
) -> Result<ArchiveManifest, ZipReadError> {
    with_validated_archive(file, cancelled, |_archive, nodes| {
        Ok(ArchiveManifest {
            nodes: nodes.to_vec(),
        })
    })
    .map_err(|error: ZipReadError| error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn inspect_returns_detached_metadata_for_explicit_implicit_and_zip64_entries() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mixed.zip");
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.add_directory("visible/", stored).unwrap();
        writer.add_directory("empty/", stored).unwrap();
        writer.start_file("root.txt", stored).unwrap();
        writer.write_all(b"root").unwrap();
        writer.start_file("visible/deep.txt", stored).unwrap();
        writer.write_all(b"nested").unwrap();
        writer.start_file("implicit/child.bin", stored).unwrap();
        writer.write_all(b"child").unwrap();
        writer
            .start_file("forced64.txt", stored.large_file(true))
            .unwrap();
        writer.write_all(b"zip64").unwrap();
        writer.finish().unwrap();

        let original = std::fs::read(&path).unwrap();
        let manifest = inspect(&mut File::open(&path).unwrap(), &AtomicBool::new(false)).unwrap();
        let describe = |node: &ManifestNode| {
            (
                node.components.join("/"),
                node.kind,
                node.explicit,
                node.entry_index,
                node.declared_uncompressed_size,
            )
        };
        let entries = manifest.nodes.iter().map(describe).collect::<Vec<_>>();
        assert!(entries.contains(&("root.txt".into(), EntryKind::File, true, Some(2), Some(4))));
        assert!(entries.contains(&("visible".into(), EntryKind::Directory, true, None, None)));
        assert!(entries.contains(&(
            "visible/deep.txt".into(),
            EntryKind::File,
            true,
            Some(3),
            Some(6)
        )));
        assert!(entries.contains(&("implicit".into(), EntryKind::Directory, false, None, None)));
        assert!(entries.contains(&(
            "implicit/child.bin".into(),
            EntryKind::File,
            true,
            Some(4),
            Some(5)
        )));
        assert!(entries.contains(&("empty".into(), EntryKind::Directory, true, None, None)));
        assert!(entries.contains(&(
            "forced64.txt".into(),
            EntryKind::File,
            true,
            Some(5),
            Some(5)
        )));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn inspect_does_not_decode_payload_and_preserves_source_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("corrupt-payload.zip");
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        writer
            .start_file(
                "payload.bin",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        let payload = b"payload bytes are not decoded by inspect";
        writer.write_all(payload).unwrap();
        writer.finish().unwrap();

        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let scanned = scan_central_directory(&mut source, &AtomicBool::new(false)).unwrap();
        let payload_start = scanned.entries[0].data_start;
        source.seek(SeekFrom::Start(payload_start)).unwrap();
        source.write_all(&[0xff]).unwrap();
        source.sync_all().unwrap();
        let corrupted_bytes = std::fs::read(&path).unwrap();

        let manifest = inspect(&mut source, &AtomicBool::new(false)).unwrap();
        assert_eq!(manifest.nodes.len(), 1);
        assert_eq!(
            manifest.nodes[0].declared_uncompressed_size,
            Some(payload.len() as u64)
        );
        assert_eq!(std::fs::read(&path).unwrap(), corrupted_bytes);
    }

    #[test]
    fn inspect_rejects_unsafe_and_colliding_real_archive_entries() {
        fn fixture(entries: &[(&str, bool)]) -> Vec<u8> {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default();
            for (name, directory) in entries {
                if *directory {
                    writer.add_directory(*name, options).unwrap();
                } else {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(b"x").unwrap();
                }
            }
            writer.finish().unwrap().into_inner()
        }

        let root = tempfile::tempdir().unwrap();
        let cases: &[(&str, &[(&str, bool)])] = &[
            ("traversal", &[("../escape.txt", false)]),
            ("reserved-alias", &[("NUL.txt", false)]),
            (
                "ordinal-parent-collision",
                &[("Foo/a.txt", false), ("foo/b.txt", false)],
            ),
            (
                "file-directory-conflict",
                &[("same", false), ("same/child.txt", false)],
            ),
        ];
        for (case, entries) in cases {
            let path = root.path().join(format!("{case}.zip"));
            std::fs::write(&path, fixture(entries)).unwrap();
            let error =
                inspect(&mut File::open(path).unwrap(), &AtomicBool::new(false)).expect_err(case);
            assert!(!error.0.is_empty(), "{case}");
        }

        let mut duplicate_directory = fixture(&[("same/", true), ("twin/", true)]);
        let mut cursor = 0;
        while let Some(relative) = duplicate_directory[cursor..]
            .windows(5)
            .position(|window| window == b"twin/")
        {
            let start = cursor + relative;
            duplicate_directory[start..start + 5].copy_from_slice(b"same/");
            cursor = start + 5;
        }
        let duplicate_path = root.path().join("duplicate-directory.zip");
        std::fs::write(&duplicate_path, duplicate_directory).unwrap();
        assert!(
            inspect(
                &mut File::open(duplicate_path).unwrap(),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
}
