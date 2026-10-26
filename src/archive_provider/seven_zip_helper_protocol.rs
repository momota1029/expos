//! Bounded transport for the isolated 7-Zip helper.
//!
//! Decoding yields untrusted descriptor metadata. It does not validate handle
//! identity against the kernel, admit archive names, or authorize file I/O.

use std::collections::HashSet;

const MAGIC: &[u8; 8] = b"GP7ZSDK\0";
const VERSION: u16 = 1;
pub(super) const HEADER_SIZE: usize = 32;
const HANDLE_SIZE: usize = 32;
const ENTRY_SIZE: usize = 24;
const MAX_ENTRIES: usize = 1024;
const MAX_HANDLES: usize = 1025;
const MAX_NAMES: usize = 2 * 1024 * 1024;
const MAX_DLL_UNITS: usize = 32767;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const DIRECTORY_SOURCE: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HandleDescriptor {
    pub role: HandleRole,
    pub raw: u64,
    pub volume_serial: u32,
    pub file_index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HandleRole {
    Output,
    Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EntryDescriptor {
    pub directory: bool,
    pub name: String,
    pub source_index: Option<u32>,
    pub expected_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Descriptor {
    pub dll_path: String,
    pub handles: Vec<HandleDescriptor>,
    pub entries: Vec<EntryDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProtocolError {
    Bounds,
    Framing,
    Encoding,
    InvalidDescriptor,
    Mapping,
}

fn absolute_windows_path(path: &str) -> bool {
    use std::path::Path;

    let bytes = path.as_bytes();
    if !Path::new(path).is_absolute() {
        return false;
    }
    if bytes.len() >= 3 && bytes[1] == b':' {
        return bytes[0].is_ascii_alphabetic() && matches!(bytes[2], b'\\' | b'/');
    }
    let normalized = path.replace('/', "\\");
    let Some(rest) = normalized.strip_prefix("\\\\") else {
        return false;
    };
    let mut components = rest.split('\\').filter(|part| !part.is_empty());
    components.next().is_some() && components.next().is_some()
}

pub(super) fn valid_raw(raw: u64) -> bool {
    raw != 0
        && raw <= usize::MAX as u64
        && !(usize::MAX as u64 - 5..=usize::MAX as u64).contains(&raw)
}

fn validate(d: &Descriptor) -> Result<(), ProtocolError> {
    if d.entries.is_empty()
        || d.entries.len() > MAX_ENTRIES
        || d.handles.is_empty()
        || d.handles.len() > MAX_HANDLES
        || d.handles[0].role != HandleRole::Output
        || d.handles[1..].iter().any(|h| h.role != HandleRole::Source)
        || d.dll_path.is_empty()
        || d.dll_path.encode_utf16().count() > MAX_DLL_UNITS
        || d.dll_path.encode_utf16().any(|u| u == 0)
        || !absolute_windows_path(&d.dll_path)
    {
        return Err(ProtocolError::InvalidDescriptor);
    }
    let mut raw_values = HashSet::new();
    if d.handles
        .iter()
        .any(|h| !valid_raw(h.raw) || !raw_values.insert(h.raw))
    {
        return Err(ProtocolError::InvalidDescriptor);
    }
    let mut used = vec![false; d.handles.len()];
    let mut names = 0usize;
    for e in &d.entries {
        names = names
            .checked_add(e.name.len())
            .ok_or(ProtocolError::Bounds)?;
        if names > MAX_NAMES || e.name.is_empty() {
            return Err(ProtocolError::Bounds);
        }
        if e.directory {
            if e.source_index.is_some() || e.expected_length != 0 {
                return Err(ProtocolError::InvalidDescriptor);
            }
        } else {
            let i = e.source_index.ok_or(ProtocolError::InvalidDescriptor)? as usize;
            if i == 0 || i >= d.handles.len() || used[i] {
                return Err(ProtocolError::InvalidDescriptor);
            }
            used[i] = true;
        }
    }
    if used.iter().skip(1).any(|v| !v) {
        return Err(ProtocolError::InvalidDescriptor);
    }
    Ok(())
}

pub(super) fn encode(d: &Descriptor) -> Result<Vec<u8>, ProtocolError> {
    validate(d)?;
    let dll: Vec<u16> = d.dll_path.encode_utf16().collect();
    let names: usize = d.entries.iter().try_fold(0usize, |n, e| {
        n.checked_add(e.name.len()).ok_or(ProtocolError::Bounds)
    })?;
    let len = HEADER_SIZE
        .checked_add(dll.len().checked_mul(2).ok_or(ProtocolError::Bounds)?)
        .and_then(|n| n.checked_add(d.handles.len() * HANDLE_SIZE))
        .and_then(|n| n.checked_add(d.entries.len() * ENTRY_SIZE))
        .and_then(|n| n.checked_add(names))
        .ok_or(ProtocolError::Bounds)?;
    if len > MAX_FRAME {
        return Err(ProtocolError::Bounds);
    }
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    put_u32(&mut out, len as u32);
    put_u32(&mut out, d.entries.len() as u32);
    put_u32(&mut out, d.handles.len() as u32);
    put_u32(&mut out, dll.len() as u32);
    put_u32(&mut out, 0);
    for u in dll {
        out.extend_from_slice(&u.to_le_bytes());
    }
    for h in &d.handles {
        out.push(match h.role {
            HandleRole::Output => 1,
            HandleRole::Source => 2,
        });
        out.extend_from_slice(&[0; 7]);
        out.extend_from_slice(&h.raw.to_le_bytes());
        put_u32(&mut out, h.volume_serial);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&h.file_index.to_le_bytes());
    }
    for e in &d.entries {
        out.push(if e.directory { 2 } else { 1 });
        out.extend_from_slice(&[0; 3]);
        put_u32(&mut out, e.name.len() as u32);
        put_u32(&mut out, e.source_index.unwrap_or(DIRECTORY_SOURCE));
        put_u32(&mut out, 0);
        out.extend_from_slice(&e.expected_length.to_le_bytes());
        out.extend_from_slice(e.name.as_bytes());
    }
    debug_assert_eq!(out.len(), len);
    Ok(out)
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.p.checked_add(n).ok_or(ProtocolError::Bounds)?;
        let s = self.b.get(self.p..end).ok_or(ProtocolError::Framing)?;
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

pub(super) fn decode(bytes: &[u8]) -> Result<Descriptor, ProtocolError> {
    if bytes.len() < HEADER_SIZE || bytes.len() > MAX_FRAME {
        return Err(ProtocolError::Bounds);
    }
    let mut c = Cursor { b: bytes, p: 0 };
    if c.take(8)? != MAGIC || c.u16()? != VERSION || c.u16()? as usize != HEADER_SIZE {
        return Err(ProtocolError::Framing);
    }
    let total = c.u32()? as usize;
    let ec = c.u32()? as usize;
    let hc = c.u32()? as usize;
    let dc = c.u32()? as usize;
    if c.u32()? != 0
        || total != bytes.len()
        || !(1..=MAX_ENTRIES).contains(&ec)
        || !(1..=MAX_HANDLES).contains(&hc)
        || dc == 0
        || dc > MAX_DLL_UNITS
    {
        return Err(ProtocolError::Bounds);
    }
    let dll_bytes = c.take(dc.checked_mul(2).ok_or(ProtocolError::Bounds)?)?;
    let units: Vec<u16> = dll_bytes
        .chunks_exact(2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .collect();
    if units.contains(&0) {
        return Err(ProtocolError::Encoding);
    }
    let dll_path = String::from_utf16(&units).map_err(|_| ProtocolError::Encoding)?;
    let mut handles = Vec::with_capacity(hc);
    for _ in 0..hc {
        let role = match c.u8()? {
            1 => HandleRole::Output,
            2 => HandleRole::Source,
            _ => return Err(ProtocolError::InvalidDescriptor),
        };
        if c.take(7)?.iter().any(|b| *b != 0) {
            return Err(ProtocolError::Framing);
        }
        let raw = c.u64()?;
        let volume_serial = c.u32()?;
        if c.take(4)?.iter().any(|b| *b != 0) {
            return Err(ProtocolError::Framing);
        }
        let file_index = c.u64()?;
        handles.push(HandleDescriptor {
            role,
            raw,
            volume_serial,
            file_index,
        });
    }
    let mut entries = Vec::with_capacity(ec);
    let mut decoded_names = 0usize;
    for _ in 0..ec {
        let kind = c.u8()?;
        if c.take(3)?.iter().any(|b| *b != 0) {
            return Err(ProtocolError::Framing);
        }
        let nl = c.u32()? as usize;
        decoded_names = decoded_names.checked_add(nl).ok_or(ProtocolError::Bounds)?;
        if decoded_names > MAX_NAMES {
            return Err(ProtocolError::Bounds);
        }
        let src = c.u32()?;
        if c.u32()? != 0 {
            return Err(ProtocolError::Framing);
        }
        let expected_length = c.u64()?;
        let name = std::str::from_utf8(c.take(nl)?)
            .map_err(|_| ProtocolError::Encoding)?
            .to_owned();
        let directory = match kind {
            1 => false,
            2 => true,
            _ => return Err(ProtocolError::InvalidDescriptor),
        };
        entries.push(EntryDescriptor {
            directory,
            name,
            source_index: if src == DIRECTORY_SOURCE {
                None
            } else {
                Some(src)
            },
            expected_length,
        });
    }
    if c.p != bytes.len() {
        return Err(ProtocolError::Framing);
    }
    let d = Descriptor {
        dll_path,
        handles,
        entries,
    };
    validate(&d)?;
    Ok(d)
}

impl Descriptor {
    pub(super) fn rejects_mapping_handle(&self, raw: u64) -> bool {
        self.handles.iter().any(|h| h.raw == raw)
    }
}

#[cfg(windows)]
mod mapping {
    use super::{HEADER_SIZE, MAX_FRAME, ProtocolError, decode};
    use std::{
        ffi::c_void,
        os::windows::io::{AsRawHandle, BorrowedHandle},
    };
    use windows::Win32::{
        Foundation::{CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE},
        System::Diagnostics::Debug::ReadProcessMemory,
        System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
            MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
        },
        System::Threading::GetCurrentProcess,
    };

    /// Owns the non-inheritable, read-only duplicate of a finalized mapping.
    /// The section is access-limited through this handle, not protected from unrelated privileged code.
    pub struct FinalizedMapping(HANDLE);
    unsafe impl Send for FinalizedMapping {}
    impl Drop for FinalizedMapping {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    impl FinalizedMapping {
        pub fn as_handle(&self) -> BorrowedHandle<'_> {
            // SAFETY: 借用が続く間、この kernel handle は self が保持する。
            unsafe { BorrowedHandle::borrow_raw(self.0.0) }
        }
        #[cfg(test)]
        pub fn read_descriptor(&self) -> Result<super::Descriptor, ProtocolError> {
            read_mapping(self.as_handle())
        }
        #[cfg(test)]
        pub(super) fn handle_value(&self) -> u64 {
            self.0.0 as usize as u64
        }
    }
    struct View(MEMORY_MAPPED_VIEW_ADDRESS);
    impl Drop for View {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(self.0);
            }
        }
    }

    pub fn create(frame: &[u8]) -> Result<FinalizedMapping, ProtocolError> {
        if frame.is_empty() || frame.len() > MAX_FRAME {
            return Err(ProtocolError::Bounds);
        }
        let size = frame.len() as u32;
        let writable = unsafe {
            CreateFileMappingW(
                HANDLE(-1isize as *mut c_void),
                None,
                PAGE_READWRITE,
                0,
                size,
                None,
            )
            .map_err(|_| ProtocolError::Mapping)?
        };
        let result = (|| unsafe {
            let view = View(MapViewOfFile(writable, FILE_MAP_WRITE, 0, 0, frame.len()));
            if view.0.Value.is_null() {
                return Err(ProtocolError::Mapping);
            }
            std::ptr::copy_nonoverlapping(frame.as_ptr(), view.0.Value.cast::<u8>(), frame.len());
            drop(view);
            let mut read = HANDLE::default();
            DuplicateHandle(
                GetCurrentProcess(),
                writable,
                GetCurrentProcess(),
                &mut read,
                FILE_MAP_READ.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .map_err(|_| ProtocolError::Mapping)?;
            Ok(FinalizedMapping(read))
        })();
        unsafe {
            let _ = CloseHandle(writable);
        }
        result
    }

    fn copy_view(view: MEMORY_MAPPED_VIEW_ADDRESS, len: usize) -> Result<Vec<u8>, ProtocolError> {
        let mut bytes = vec![0u8; len];
        let mut bytes_read = 0usize;
        // SAFETY: 読み取り元は生ポインターのまま渡し、有効範囲の確認は OS の API に任せる。
        // bytes は len byte の初期化済み書き込み可能領域で、共有領域への Rust 参照は作らない。
        // 本番の view は呼び出し側が保持し、テストでは無効 address の拒否も確認する。
        let copied = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                view.Value.cast_const(),
                bytes.as_mut_ptr().cast(),
                len,
                Some(&mut bytes_read),
            )
        }
        .is_ok();
        require_full_copy(copied, bytes_read, bytes)
    }

    fn require_full_copy(
        succeeded: bool,
        bytes_read: usize,
        bytes: Vec<u8>,
    ) -> Result<Vec<u8>, ProtocolError> {
        if !succeeded || bytes_read != bytes.len() {
            return Err(ProtocolError::Framing);
        }
        Ok(bytes)
    }

    /// decode 前に mapping の byte を独立させるが、descriptor は引き続き未認可の入力。
    /// copied header の一致は、同時書き込み中の frame の atomic 性や真正性を保証しない。
    pub fn read_mapping(handle: BorrowedHandle<'_>) -> Result<super::Descriptor, ProtocolError> {
        // SAFETY: 呼び出し中の handle 生存は BorrowedHandle が保証する。
        let handle = HANDLE(handle.as_raw_handle().cast());
        unsafe {
            let first = View(MapViewOfFile(handle, FILE_MAP_READ, 0, 0, HEADER_SIZE));
            if first.0.Value.is_null() {
                return Err(ProtocolError::Framing);
            }
            let header: [u8; HEADER_SIZE] = copy_view(first.0, HEADER_SIZE)?
                .try_into()
                .map_err(|_| ProtocolError::Framing)?;
            let total = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            if total < HEADER_SIZE || total > MAX_FRAME {
                return Err(ProtocolError::Bounds);
            }
            drop(first);
            let full = View(MapViewOfFile(handle, FILE_MAP_READ, 0, 0, total));
            if full.0.Value.is_null() {
                return Err(ProtocolError::Framing);
            }
            let bytes = copy_view(full.0, total)?;
            decode_copied_frame(&header, bytes, handle.0 as usize as u64)
        }
    }

    fn decode_copied_frame(
        stable_header: &[u8; HEADER_SIZE],
        bytes: Vec<u8>,
        mapping_handle: u64,
    ) -> Result<super::Descriptor, ProtocolError> {
        if bytes.get(..HEADER_SIZE) != Some(stable_header.as_slice()) {
            return Err(ProtocolError::Framing);
        }
        let descriptor = decode(&bytes)?;
        if descriptor.rejects_mapping_handle(mapping_handle) {
            return Err(ProtocolError::InvalidDescriptor);
        }
        Ok(descriptor)
    }

    #[cfg(test)]
    mod copy_tests {
        use super::*;

        #[test]
        fn mapping_copy_rejects_api_failure_and_short_success() {
            assert_eq!(
                require_full_copy(false, 4, vec![0; 4]),
                Err(ProtocolError::Framing)
            );
            assert_eq!(
                require_full_copy(true, 3, vec![0; 4]),
                Err(ProtocolError::Framing)
            );
            assert_eq!(require_full_copy(true, 4, vec![0; 4]), Ok(vec![0; 4]));
            assert_eq!(
                copy_view(MEMORY_MAPPED_VIEW_ADDRESS::default(), 1),
                Err(ProtocolError::Framing),
                "ReadProcessMemory must reject an inaccessible source"
            );
        }

        fn valid_descriptor(name: &str) -> super::super::Descriptor {
            super::super::Descriptor {
                dll_path: r"C:\Program Files\7-Zip\7z.dll".into(),
                handles: vec![
                    super::super::HandleDescriptor {
                        role: super::super::HandleRole::Output,
                        raw: 0x10001,
                        volume_serial: 7,
                        file_index: 10,
                    },
                    super::super::HandleDescriptor {
                        role: super::super::HandleRole::Source,
                        raw: 0x10002,
                        volume_serial: 7,
                        file_index: 11,
                    },
                ],
                entries: vec![super::super::EntryDescriptor {
                    directory: false,
                    name: name.into(),
                    source_index: Some(1),
                    expected_length: 1,
                }],
            }
        }

        #[test]
        fn copied_mapping_frame_rejects_changed_header() {
            let first = super::super::encode(&valid_descriptor("a")).unwrap();
            let stable: [u8; HEADER_SIZE] = first[..HEADER_SIZE].try_into().unwrap();
            let changed = super::super::encode(&valid_descriptor("different name")).unwrap();
            assert!(super::super::decode(&changed).is_ok());
            assert_ne!(&changed[..HEADER_SIZE], &stable);
            assert_eq!(
                decode_copied_frame(&stable, changed, 0x20000),
                Err(ProtocolError::Framing)
            );
        }
    }
}

#[cfg(windows)]
pub(super) use mapping::{FinalizedMapping, create as create_mapping, read_mapping};

#[cfg(test)]
mod tests {
    use super::*;
    fn put_u32_at(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u64_at(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn offsets(bytes: &[u8]) -> (usize, usize) {
        let dll_units = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;
        let handles = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        (
            HEADER_SIZE + dll_units * 2,
            HEADER_SIZE + dll_units * 2 + handles * HANDLE_SIZE,
        )
    }
    fn sample() -> Descriptor {
        Descriptor {
            dll_path: r"C:\Program Files\7-Zip\7z.dll".into(),
            handles: vec![
                HandleDescriptor {
                    role: HandleRole::Output,
                    raw: 0x10001,
                    volume_serial: 7,
                    file_index: 10,
                },
                HandleDescriptor {
                    role: HandleRole::Source,
                    raw: 0x10002,
                    volume_serial: 7,
                    file_index: 11,
                },
            ],
            entries: vec![
                EntryDescriptor {
                    directory: true,
                    name: "empty 📁".into(),
                    source_index: None,
                    expected_length: 0,
                },
                EntryDescriptor {
                    directory: false,
                    name: "@- 名.txt".into(),
                    source_index: Some(1),
                    expected_length: 9,
                },
            ],
        }
    }
    #[test]
    fn roundtrip_unicode_and_empty_directory() {
        let d = sample();
        assert_eq!(decode(&encode(&d).unwrap()).unwrap(), d);
    }
    #[test]
    fn decoder_rejects_mutated_header_handle_entry_and_encoding_fields() {
        let b = encode(&sample()).unwrap();
        let (handles, entries) = offsets(&b);
        let mut mutations: Vec<Box<dyn Fn(&mut Vec<u8>)>> = vec![
            Box::new(|x| x[0] ^= 1),
            Box::new(|x| x[8] = 2),
            Box::new(|x| x[10] = 31),
            Box::new(|x| x[12] ^= 1),
            Box::new(|x| put_u32_at(x, 16, 0)),
            Box::new(|x| put_u32_at(x, 16, MAX_ENTRIES as u32 + 1)),
            Box::new(|x| put_u32_at(x, 20, 0)),
            Box::new(|x| put_u32_at(x, 20, MAX_HANDLES as u32 + 1)),
            Box::new(|x| put_u32_at(x, 24, 0)),
            Box::new(|x| put_u32_at(x, 24, MAX_DLL_UNITS as u32 + 1)),
            Box::new(|x| put_u32_at(x, 28, 1)),
            Box::new(move |x| x[handles] = 0),
            Box::new(move |x| x[handles] = 2),
            Box::new(move |x| x[handles + 1] = 1),
            Box::new(move |x| x[handles + 20] = 1),
            Box::new(move |x| put_u64_at(x, handles + HANDLE_SIZE + 8, 0)),
            Box::new(move |x| put_u64_at(x, handles + 8, 0)),
            Box::new(move |x| put_u64_at(x, handles + 8, u64::MAX)),
            Box::new(move |x| x[entries] = 3),
            Box::new(move |x| x[entries + 1] = 1),
            Box::new(move |x| put_u32_at(x, entries + 4, MAX_NAMES as u32 + 1)),
            Box::new(move |x| put_u32_at(x, entries + 8, 0)),
            Box::new(move |x| put_u32_at(x, entries + 12, 1)),
            Box::new(move |x| put_u64_at(x, entries + 16, 1)),
        ];
        for mutate in mutations.drain(..) {
            let mut x = b.clone();
            mutate(&mut x);
            assert!(decode(&x).is_err());
        }
        let mut x = b.clone();
        put_u64_at(&mut x, handles + HANDLE_SIZE + 8, 0x10001);
        assert!(decode(&x).is_err(), "duplicate numeric handles");
        let mut x = b.clone();
        let last = x.len() - 1;
        x[last] = 0xff;
        assert!(decode(&x).is_err(), "malformed UTF-8 name");
        for truncated in [0, HEADER_SIZE - 1] {
            assert!(decode(&b[..truncated]).is_err());
        }
        let mut body_truncated = b[..b.len() - 1].to_vec();
        let body_truncated_len = body_truncated.len() as u32;
        put_u32_at(&mut body_truncated, 12, body_truncated_len);
        assert!(
            decode(&body_truncated).is_err(),
            "valid total, truncated body"
        );
        let mut trailing = b.clone();
        trailing.push(0);
        let trailing_len = trailing.len() as u32;
        put_u32_at(&mut trailing, 12, trailing_len);
        assert!(decode(&trailing).is_err());
        let mut bad_utf16 = b.clone();
        bad_utf16[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&0xd800u16.to_le_bytes());
        assert!(decode(&bad_utf16).is_err());
        for pseudo in (usize::MAX as u64 - 5)..=usize::MAX as u64 {
            let mut pseudo_handle = b.clone();
            put_u64_at(&mut pseudo_handle, handles + 8, pseudo);
            assert!(decode(&pseudo_handle).is_err(), "pseudo handle {pseudo:#x}");
        }
        if usize::BITS < 64 {
            let mut oversized = b.clone();
            put_u64_at(&mut oversized, handles + 8, usize::MAX as u64 + 1);
            assert!(decode(&oversized).is_err(), "native-width overflow");
        }
        let mut nul_dll = b.clone();
        nul_dll[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&0u16.to_le_bytes());
        assert!(decode(&nul_dll).is_err());
        let mut invalid_drive = b.clone();
        invalid_drive[HEADER_SIZE..HEADER_SIZE + 2].copy_from_slice(&(b'1' as u16).to_le_bytes());
        assert!(decode(&invalid_drive).is_err());
        let mut incomplete_unc = b.clone();
        let units = u32::from_le_bytes(incomplete_unc[24..28].try_into().unwrap()) as usize;
        for i in 0..units {
            incomplete_unc[HEADER_SIZE + i * 2..HEADER_SIZE + i * 2 + 2]
                .copy_from_slice(&(b'\\' as u16).to_le_bytes());
        }
        assert!(decode(&incomplete_unc).is_err());
    }
    #[test]
    fn decoder_rejects_bad_source_associations_and_encoder_rejects_them_too() {
        let base = encode(&sample()).unwrap();
        let (_, entries_at) = offsets(&base);
        let second_entry = entries_at + ENTRY_SIZE + sample().entries[0].name.len();
        for source_index in [0, 2, u32::MAX - 1] {
            let mut bad = base.clone();
            put_u32_at(&mut bad, second_entry + 8, source_index);
            assert!(decode(&bad).is_err());
        }

        let mut d = sample();
        d.handles.push(HandleDescriptor {
            role: HandleRole::Source,
            raw: 0x10003,
            volume_serial: 7,
            file_index: 12,
        });
        d.entries.push(EntryDescriptor {
            directory: false,
            name: "another.txt".into(),
            source_index: Some(2),
            expected_length: 3,
        });
        let valid = encode(&d).unwrap();
        let (_, entries_at) = offsets(&valid);
        let third_entry = entries_at
            + ENTRY_SIZE
            + d.entries[0].name.len()
            + ENTRY_SIZE
            + d.entries[1].name.len();
        let mut reused = valid.clone();
        put_u32_at(&mut reused, third_entry + 8, 1);
        assert!(
            decode(&reused).is_err(),
            "source index reuse and unreferenced source"
        );
        let mut unused_source = valid.clone();
        unused_source.truncate(third_entry);
        let unused_source_len = unused_source.len() as u32;
        put_u32_at(&mut unused_source, 12, unused_source_len);
        put_u32_at(&mut unused_source, 16, 2);
        assert!(decode(&unused_source).is_err(), "unused source handle");

        d.entries[1].source_index = Some(0);
        assert!(encode(&d).is_err());
        d = sample();
        d.handles.push(HandleDescriptor {
            role: HandleRole::Source,
            raw: 0x10003,
            volume_serial: 7,
            file_index: 12,
        });
        assert!(encode(&d).is_err(), "unreferenced source handle");
        d = sample();
        d.entries[0].source_index = Some(1);
        assert!(encode(&d).is_err(), "directory source handle");
        d = sample();
        d.entries[0].expected_length = 1;
        assert!(encode(&d).is_err(), "directory expected length");
        d = sample();
        d.dll_path = "1:\\bad.dll".into();
        assert!(encode(&d).is_err(), "drive prefix must be alphabetic");
        d.dll_path = "\\\\".into();
        assert!(encode(&d).is_err(), "UNC path requires server and share");
        d.dll_path = "C:\\bad\0.dll".into();
        assert!(encode(&d).is_err(), "DLL path NUL");
        d = sample();
        d.handles[1].raw = 4;
        assert!(
            encode(&d).is_ok(),
            "positive handle value four is representable"
        );
        d.handles[0].raw = 4;
        assert!(encode(&d).is_err(), "numeric handle uniqueness");
        d = sample();
        d.handles[1].volume_serial = d.handles[0].volume_serial;
        d.handles[1].file_index = d.handles[0].file_index;
        assert!(
            encode(&d).is_ok(),
            "distinct handles may share a file identity"
        );
    }

    #[test]
    fn encoder_accepts_entry_and_handle_count_limits() {
        let mut dirs = sample();
        dirs.handles.truncate(1);
        dirs.entries = (0..MAX_ENTRIES)
            .map(|i| EntryDescriptor {
                directory: true,
                name: format!("dir-{i}"),
                source_index: None,
                expected_length: 0,
            })
            .collect();
        assert!(encode(&dirs).is_ok());
        dirs.entries.push(EntryDescriptor {
            directory: true,
            name: "overflow".into(),
            source_index: None,
            expected_length: 0,
        });
        assert!(encode(&dirs).is_err());

        let mut all_sources = sample();
        all_sources.handles = (0..MAX_HANDLES)
            .map(|i| HandleDescriptor {
                role: if i == 0 {
                    HandleRole::Output
                } else {
                    HandleRole::Source
                },
                raw: 0x20000 + i as u64,
                volume_serial: 8,
                file_index: i as u64,
            })
            .collect();
        all_sources.entries = (1..MAX_HANDLES)
            .map(|i| EntryDescriptor {
                directory: false,
                name: format!("file-{i}"),
                source_index: Some(i as u32),
                expected_length: 1,
            })
            .collect();
        assert!(encode(&all_sources).is_ok());
        all_sources.handles.push(HandleDescriptor {
            role: HandleRole::Source,
            raw: 0x30000,
            volume_serial: 8,
            file_index: 1025,
        });
        assert!(encode(&all_sources).is_err());
    }
    #[test]
    fn declared_budgets_and_mapping_handle_collision() {
        let mut d = sample();
        d.dll_path = format!("C:\\{}", "x".repeat(MAX_DLL_UNITS - 3));
        assert_eq!(d.dll_path.encode_utf16().count(), MAX_DLL_UNITS);
        assert!(
            validate(&d).is_ok(),
            "absolute path: {}",
            absolute_windows_path(&d.dll_path)
        );
        assert!(encode(&d).is_ok());
        d.dll_path.push('x');
        assert!(encode(&d).is_err());
        d = sample();
        let second_name_len = d.entries[1].name.len();
        d.entries[0].name = "x".repeat(MAX_NAMES - second_name_len);
        assert!(encode(&d).is_ok());
        d.entries[0].name.push('x');
        assert!(encode(&d).is_err());
        assert!(valid_raw(4));
        for pseudo in [usize::MAX as u64 - 5, usize::MAX as u64] {
            assert!(!valid_raw(pseudo));
        }
        assert!(!absolute_windows_path("1:\\bad.dll"));
        assert!(!absolute_windows_path("\\\\"));
        assert!(!absolute_windows_path("\\\\server"));
        assert!(!sample().rejects_mapping_handle(0x77777));
        assert!(sample().rejects_mapping_handle(0x10001));
    }
    #[cfg(windows)]
    #[test]
    fn sealed_mapping_roundtrip_and_write_refusal() {
        use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::{
            Foundation::{DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE},
            System::Threading::GetCurrentProcess,
        };

        let b = encode(&sample()).unwrap();
        let m = super::mapping::create(&b).unwrap();
        assert_eq!(m.read_descriptor().unwrap(), sample());
        assert!(
            unsafe {
                windows::Win32::System::Memory::MapViewOfFile(
                    HANDLE(m.as_handle().as_raw_handle().cast()),
                    windows::Win32::System::Memory::FILE_MAP_WRITE,
                    0,
                    0,
                    1,
                )
            }
            .Value
            .is_null()
        );
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(m.as_handle().as_raw_handle().cast()),
                GetCurrentProcess(),
                &mut duplicate,
                windows::Win32::System::Memory::FILE_MAP_READ.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .unwrap();
        }
        let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate.0) };
        assert_eq!(
            super::mapping::read_mapping(duplicate.as_handle()).unwrap(),
            sample()
        );
        assert_eq!(
            m.handle_value(),
            m.as_handle().as_raw_handle() as usize as u64
        );
    }
    #[cfg(windows)]
    #[test]
    fn read_mapping_returns_detached_frames_with_writable_alias_retained() {
        use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::{
            Foundation::{DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE},
            System::{
                Memory::{
                    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
                    MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
                },
                Threading::GetCurrentProcess,
            },
        };

        struct WritableView(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for WritableView {
            fn drop(&mut self) {
                unsafe {
                    let _ = UnmapViewOfFile(self.0);
                }
            }
        }

        let first_descriptor = sample();
        let first_frame = encode(&first_descriptor).unwrap();
        let writable = unsafe {
            CreateFileMappingW(
                HANDLE(-1isize as *mut std::ffi::c_void),
                None,
                PAGE_READWRITE,
                0,
                first_frame.len() as u32,
                None,
            )
            .unwrap()
        };
        // SAFETY: 新規 mapping handle の所有権を一度だけ OwnedHandle に移す。
        let writable = unsafe { OwnedHandle::from_raw_handle(writable.0) };
        let view = unsafe {
            MapViewOfFile(
                HANDLE(writable.as_handle().as_raw_handle().cast()),
                FILE_MAP_WRITE,
                0,
                0,
                first_frame.len(),
            )
        };
        assert!(!view.Value.is_null());
        let view = WritableView(view);
        // SAFETY: view 全域が frame 長分書き込み可能で、同時に読み取る reader はいない。
        unsafe {
            std::ptr::copy_nonoverlapping(
                first_frame.as_ptr(),
                view.0.Value.cast::<u8>(),
                first_frame.len(),
            );
        }

        let mut readonly = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(writable.as_handle().as_raw_handle().cast()),
                GetCurrentProcess(),
                &mut readonly,
                FILE_MAP_READ.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .unwrap();
        }
        // SAFETY: DuplicateHandle が返した新規 mapping handle の所有権を一度だけ移す。
        let readonly = unsafe { OwnedHandle::from_raw_handle(readonly.0) };
        let first_read = super::mapping::read_mapping(readonly.as_handle()).unwrap();
        assert_eq!(first_read, first_descriptor);

        let mut second_descriptor = first_descriptor.clone();
        second_descriptor.entries[1].name = "@- 名.bin".into();
        let second_frame = encode(&second_descriptor).unwrap();
        assert_eq!(second_frame.len(), first_frame.len());
        // SAFETY: writable alias は生存中で、最初の read が完了してから書き換える。
        unsafe {
            std::ptr::copy_nonoverlapping(
                second_frame.as_ptr(),
                view.0.Value.cast::<u8>(),
                second_frame.len(),
            );
        }
        let second_read = super::mapping::read_mapping(readonly.as_handle()).unwrap();
        assert_eq!(second_read, second_descriptor);
        assert_eq!(
            first_read, first_descriptor,
            "earlier decode owns detached bytes"
        );
    }
    #[cfg(windows)]
    #[test]
    fn truncated_mapping_and_mapping_collision_are_rejected() {
        use std::os::windows::io::{AsHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::{
            Foundation::{CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE},
            System::{
                Memory::{
                    CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READWRITE,
                    UnmapViewOfFile,
                },
                Threading::GetCurrentProcess,
            },
        };

        let mut too_small_total = encode(&sample()).unwrap();
        put_u32_at(&mut too_small_total, 12, (HEADER_SIZE - 1) as u32);
        let undersized = super::mapping::create(&too_small_total).unwrap();
        assert_eq!(undersized.read_descriptor(), Err(ProtocolError::Bounds));
        let mut too_large_total = encode(&sample()).unwrap();
        put_u32_at(&mut too_large_total, 12, MAX_FRAME as u32 + 1);
        let oversized = super::mapping::create(&too_large_total).unwrap();
        assert_eq!(oversized.read_descriptor(), Err(ProtocolError::Bounds));

        let mut long = sample();
        long.entries[0].name = "x".repeat(8192);
        let intended = encode(&long).unwrap();
        let short = super::mapping::create(&intended[..HEADER_SIZE]).unwrap();
        // Pagefile section は page 単位へ丸められるため、短い超過だけでは拒否を証明できない。
        // 1 page を超える正常な frame 長を宣言し、2回目の view 取得で範囲外を確実に検出する。
        assert!(short.read_descriptor().is_err());

        let seed = encode(&sample()).unwrap();
        let writable = unsafe {
            CreateFileMappingW(
                HANDLE(-1isize as *mut std::ffi::c_void),
                None,
                PAGE_READWRITE,
                0,
                seed.len() as u32,
                None,
            )
            .unwrap()
        };
        let mut readonly = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                writable,
                GetCurrentProcess(),
                &mut readonly,
                FILE_MAP_READ.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .unwrap();
        }
        let mut collided = sample();
        collided.handles[0].raw = readonly.0 as usize as u64;
        let collision_frame = encode(&collided).unwrap();
        let view = unsafe {
            MapViewOfFile(
                writable,
                windows::Win32::System::Memory::FILE_MAP_WRITE,
                0,
                0,
                collision_frame.len(),
            )
        };
        assert!(!view.Value.is_null());
        unsafe {
            std::ptr::copy_nonoverlapping(
                collision_frame.as_ptr(),
                view.Value.cast::<u8>(),
                collision_frame.len(),
            );
            UnmapViewOfFile(view).unwrap();
        }
        unsafe {
            CloseHandle(writable).unwrap();
        }
        let readonly = unsafe { OwnedHandle::from_raw_handle(readonly.0) };
        assert_eq!(
            super::mapping::read_mapping(readonly.as_handle()),
            Err(ProtocolError::InvalidDescriptor)
        );
    }
}
