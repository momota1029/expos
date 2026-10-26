#![cfg(windows)]
#![allow(unsafe_op_in_unsafe_fn)]

mod seven_zip_extract_sdk;

use std::{
    ffi::c_void,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE, S_OK},
        System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            LoadLibraryExW,
        },
    },
    core::{GUID, PCWSTR},
};

use crate::archive_manifest::{NodeKind, RawRecord};

const E_FAIL: i32 = 0x8000_4005u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const VT_EMPTY_U16: u16 = 0;
const VT_BOOL_U16: u16 = 11;
const VT_UI4_U16: u16 = 19;
const VT_UI8_U16: u16 = 21;
const MAX_RECORDS: usize = 65_536;
const MAX_DEPTH: usize = 128;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
// 7-Zip 24.09 の PropID.h にある kpid 値を SDK の実値で固定する。
const KPID_PATH: u32 = 3;
const KPID_IS_DIR: u32 = 6;
const KPID_SIZE: u32 = 7;
const KPID_ATTRIBUTES: u32 = 9;
const KPID_ENCRYPTED: u32 = 15;
const KPID_IS_ANTI: u32 = 21;
const KPID_OFFSET: u32 = 36;
const KPID_PHYSICAL_SIZE: u32 = 44;
const KPID_POSIX_ATTRIBUTES: u32 = 53;
const KPID_SYMLINK: u32 = 54;
const KPID_IS_ALT_STREAM: u32 = 63;
const KPID_ERROR_FLAGS: u32 = 71;
const KPID_WARNING_FLAGS: u32 = 72;
const KPID_CRC: u32 = 19;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x0000000000000000c000000000000046);
const IID_I_IN_STREAM: GUID = sdk_iid(3, 3);
const IID_I_SEQUENTIAL_OUT_STREAM: GUID = sdk_iid(3, 2);
const IID_I_PROGRESS: GUID = sdk_iid(0, 5);
const IID_I_ARCHIVE_EXTRACT_CALLBACK: GUID = sdk_iid(6, 0x20);
const IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2: GUID = sdk_iid(6, 0x22);
const IID_I_IN_ARCHIVE: GUID = sdk_iid(6, 0x60);
const IID_I_ARCHIVE_RAW_PROPS: GUID = sdk_iid(6, 0x70);
const CLSID_7Z_FORMAT: GUID = GUID::from_u128(0x23170f6940c1278a1000000110070000);

const fn sdk_iid(group: u16, interface: u8) -> GUID {
    // SDK の Guid.txt に記載された group/interface を GUID の末尾へ対応させる。
    GUID {
        data1: 0x23170F69,
        data2: 0x40C1,
        data3: 0x278A,
        data4: [0, 0, 0, group as u8, (group >> 8) as u8, interface, 0, 0],
    }
}

#[repr(C)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    value: [u64; 2],
}
impl PropVariant {
    const EMPTY: Self = Self {
        vt: VT_EMPTY_U16,
        reserved: [0; 3],
        value: [0; 2],
    };
    fn u64(&self) -> Option<u64> {
        (self.vt == VT_UI8_U16).then_some(self.value[0])
    }
    fn u32(&self) -> Option<u32> {
        (self.vt == VT_UI4_U16).then_some(self.value[0] as u32)
    }
    fn boolean(&self) -> Option<bool> {
        if self.vt != VT_BOOL_U16 {
            return None;
        }
        let v = self.value[0] as u16;
        (v == 0 || v == u16::MAX).then_some(v != 0)
    }
}
impl Drop for PropVariant {
    fn drop(&mut self) {
        unsafe {
            let _ = prop_variant_clear(self);
        }
    }
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn PropVariantClear(value: *mut PropVariant) -> i32;
}

unsafe fn prop_variant_clear(value: *mut PropVariant) -> i32 {
    unsafe { PropVariantClear(value) }
}

#[repr(C)]
struct UnknownVtable {
    query_interface: unsafe extern "system" fn(*mut Unknown, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Unknown) -> u32,
    release: unsafe extern "system" fn(*mut Unknown) -> u32,
}
#[repr(C)]
struct Unknown {
    vtable: *const UnknownVtable,
}

#[repr(C)]
struct InSeqVtable {
    unknown: UnknownVtable,
    read: unsafe extern "system" fn(*mut InStream, *mut c_void, u32, *mut u32) -> i32,
}
#[repr(C)]
struct InStreamVtable {
    seq: InSeqVtable,
    seek: unsafe extern "system" fn(*mut InStream, i64, u32, *mut u64) -> i32,
}
#[repr(C)]
struct InStream {
    vtable: *const InStreamVtable,
    state: Box<InputState>,
    refs: AtomicU32,
}

type ArchiveExtractFn =
    unsafe extern "system" fn(*mut Archive, *const u32, u32, i32, *mut Unknown) -> i32;

#[repr(C)]
struct ArchiveVtable {
    unknown: UnknownVtable,
    open: unsafe extern "system" fn(*mut Archive, *mut InStream, *const u64, *mut c_void) -> i32,
    close: unsafe extern "system" fn(*mut Archive) -> i32,
    get_number_of_items: unsafe extern "system" fn(*mut Archive, *mut u32) -> i32,
    get_property: unsafe extern "system" fn(*mut Archive, u32, u32, *mut PropVariant) -> i32,
    extract: ArchiveExtractFn,
    get_archive_property: unsafe extern "system" fn(*mut Archive, u32, *mut PropVariant) -> i32,
}
#[repr(C)]
struct Archive {
    vtable: *const ArchiveVtable,
}
#[repr(C)]
struct RawPropsVtable {
    unknown: UnknownVtable,
    get_parent: unsafe extern "system" fn(*mut RawProps, u32, *mut u32, *mut u32) -> i32,
    get_raw_prop: unsafe extern "system" fn(
        *mut RawProps,
        u32,
        u32,
        *mut *const c_void,
        *mut u32,
        *mut u32,
    ) -> i32,
    get_num_raw_props: unsafe extern "system" fn(*mut RawProps, *mut u32) -> i32,
    get_raw_prop_info:
        unsafe extern "system" fn(*mut RawProps, u32, *mut *mut u16, *mut u32) -> i32,
}
#[repr(C)]
struct RawProps {
    vtable: *const RawPropsVtable,
}

struct ComPtr(*mut Unknown);
impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                ((*(*self.0).vtable).release)(self.0);
            }
        }
    }
}
struct Module(HMODULE);
impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}
impl Module {
    fn load(dll: &Path) -> Result<Self, i32> {
        let wide: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            LoadLibraryExW(
                PCWSTR(wide.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map(Self)
        .map_err(|_| E_FAIL)
    }
}
type CreateObjectFn = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;

struct InputState {
    file: File,
    length: u64,
    failed: AtomicBool,
}

unsafe extern "system" fn stream_query(
    this: *mut Unknown,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    *out = ptr::null_mut();
    if *iid != IID_IUNKNOWN && *iid != IID_I_IN_STREAM {
        return 0x8000_4002u32 as i32;
    }
    stream_add_ref(this);
    *out = this.cast();
    S_OK.0
}
unsafe extern "system" fn stream_add_ref(this: *mut Unknown) -> u32 {
    let stream = this.cast::<InStream>();
    (*stream).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn stream_release(this: *mut Unknown) -> u32 {
    let stream = this.cast::<InStream>();
    let remaining = (*stream).refs.fetch_sub(1, Ordering::Release) - 1;
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(stream));
    }
    remaining
}
unsafe extern "system" fn stream_read(
    this: *mut InStream,
    dst: *mut c_void,
    count: u32,
    read: *mut u32,
) -> i32 {
    if !read.is_null() {
        *read = 0;
    }
    if dst.is_null() && count != 0 || read.is_null() {
        return E_INVALIDARG;
    }
    if count == 0 {
        return S_OK.0;
    }
    let state = &mut (*this).state;
    if state.failed.load(Ordering::Acquire) {
        return E_FAIL;
    }
    let pos = match state.file.stream_position() {
        Ok(p) => p,
        Err(_) => {
            state.failed.store(true, Ordering::Release);
            return E_FAIL;
        }
    };
    if pos > state.length {
        state.failed.store(true, Ordering::Release);
        return E_FAIL;
    }
    let take = (count as u64).min(state.length - pos) as usize;
    let target = std::slice::from_raw_parts_mut(dst.cast::<u8>(), take);
    match state.file.read(target) {
        Ok(n) => {
            *read = n as u32;
            S_OK.0
        }
        Err(_) => {
            state.failed.store(true, Ordering::Release);
            E_FAIL
        }
    }
}
unsafe extern "system" fn stream_seek(
    this: *mut InStream,
    offset: i64,
    origin: u32,
    new_pos: *mut u64,
) -> i32 {
    let state = &mut (*this).state;
    if state.failed.load(Ordering::Acquire) {
        return E_FAIL;
    }
    let base = match origin {
        0 => 0i128,
        1 => match state.file.stream_position() {
            Ok(pos) => pos as i128,
            Err(_) => {
                state.failed.store(true, Ordering::Release);
                return E_FAIL;
            }
        },
        2 => state.length as i128,
        _ => return E_INVALIDARG,
    };
    let target = base + offset as i128;
    if target < 0 || target > state.length as i128 {
        state.failed.store(true, Ordering::Release);
        return E_FAIL;
    }
    match state.file.seek(SeekFrom::Start(target as u64)) {
        Ok(pos) => {
            if !new_pos.is_null() {
                *new_pos = pos;
            }
            S_OK.0
        }
        Err(_) => {
            state.failed.store(true, Ordering::Release);
            E_FAIL
        }
    }
}
static STREAM_VTABLE: InStreamVtable = InStreamVtable {
    seq: InSeqVtable {
        unknown: UnknownVtable {
            query_interface: stream_query,
            add_ref: stream_add_ref,
            release: stream_release,
        },
        read: stream_read,
    },
    seek: stream_seek,
};

pub(super) fn read_archive(
    dll: &Path,
    source: File,
    expected_length: u64,
) -> Result<Vec<RawRecord>, i32> {
    read_archive_records(dll, source, expected_length, false).map(|records| {
        records
            .into_iter()
            .map(|record| RawRecord {
                components: record.components,
                kind: record.kind,
                declared_size: record.declared_size,
            })
            .collect()
    })
}

pub(super) fn inspect_archive(
    dll: &Path,
    source: File,
    expected_length: u64,
) -> Result<Vec<super::seven_zip_extract_plan::IndexedRecord>, i32> {
    read_archive_records(dll, source, expected_length, true)
}

pub(super) fn extract_archive(
    dll: &Path,
    source: File,
    expected_length: u64,
    plan: Arc<super::seven_zip_extract_plan::Validated7zExtractPlan>,
    spool: File,
    cancelled: Arc<AtomicBool>,
) -> Result<(), i32> {
    seven_zip_extract_sdk::extract_archive(dll, source, expected_length, plan, spool, cancelled)
}

fn read_archive_records(
    dll: &Path,
    source: File,
    expected_length: u64,
    include_crc: bool,
) -> Result<Vec<super::seven_zip_extract_plan::IndexedRecord>, i32> {
    read_archive_records_with(
        dll,
        source,
        expected_length,
        include_crc,
        |_, _| Ok(()),
        |_, source_failed| {
            if source_failed { Err(E_FAIL) } else { Ok(()) }
        },
    )
}

fn read_archive_records_with<T>(
    dll: &Path,
    source: File,
    expected_length: u64,
    include_crc: bool,
    after_inventory: impl FnOnce(
        *mut Archive,
        &[super::seven_zip_extract_plan::IndexedRecord],
    ) -> Result<T, i32>,
    finalize: impl FnOnce(T, bool) -> Result<(), i32>,
) -> Result<Vec<super::seven_zip_extract_plan::IndexedRecord>, i32> {
    if !dll.is_absolute() || dll.as_os_str().encode_wide().any(|u| u == 0) {
        return Err(E_INVALIDARG);
    }
    if source.metadata().map_err(|_| E_FAIL)?.len() != expected_length {
        return Err(E_INVALIDARG);
    }
    let module = Module::load(dll)?;
    let mut source = source;
    source.seek(SeekFrom::Start(0)).map_err(|_| E_FAIL)?;
    let stream = Box::new(InStream {
        vtable: &STREAM_VTABLE,
        state: Box::new(InputState {
            file: source,
            length: expected_length,
            failed: AtomicBool::new(false),
        }),
        refs: AtomicU32::new(1),
    });
    let stream_ptr = Box::into_raw(stream);
    let stream_owner = ComPtr(stream_ptr.cast());
    let proc =
        unsafe { GetProcAddress(module.0, windows::core::s!("CreateObject")) }.ok_or(E_FAIL)?;
    let create: CreateObjectFn = unsafe { std::mem::transmute(proc) };
    let mut raw = ptr::null_mut();
    exact(unsafe { create(&CLSID_7Z_FORMAT, &IID_I_IN_ARCHIVE, &mut raw) })?;
    if raw.is_null() {
        return Err(E_FAIL);
    }
    let archive = raw.cast::<Archive>();
    let archive_owner = ComPtr(raw.cast());
    let zero = 0u64;
    exact(unsafe { ((*(*archive).vtable).open)(archive, stream_ptr, &zero, ptr::null_mut()) })?;
    let _close = ArchiveClose(archive);
    let mut archive_offset = PropVariant::EMPTY;
    let mut physical_size = PropVariant::EMPTY;
    let mut errors = PropVariant::EMPTY;
    let mut warnings = PropVariant::EMPTY;
    for (id, prop) in [
        (KPID_OFFSET, &mut archive_offset),
        (KPID_PHYSICAL_SIZE, &mut physical_size),
        (KPID_ERROR_FLAGS, &mut errors),
        (KPID_WARNING_FLAGS, &mut warnings),
    ] {
        exact(unsafe { ((*(*archive).vtable).get_archive_property)(archive, id, prop) })?;
    }
    if optional_numeric_zero(&archive_offset) != Some(0)
        || numeric(&physical_size) != Some(expected_length)
        || optional_numeric_zero(&errors) != Some(0)
        || optional_numeric_zero(&warnings) != Some(0)
    {
        return Err(E_FAIL);
    }
    let mut count = 0u32;
    exact(unsafe { ((*(*archive).vtable).get_number_of_items)(archive, &mut count) })?;
    if count as usize > MAX_RECORDS {
        return Err(E_FAIL);
    }
    let mut raw_props = ptr::null_mut();
    exact(unsafe {
        ((*(*archive).vtable).unknown.query_interface)(
            archive.cast(),
            &IID_I_ARCHIVE_RAW_PROPS,
            &mut raw_props,
        )
    })?;
    if raw_props.is_null() {
        return Err(E_FAIL);
    }
    let raw_owner = ComPtr(raw_props.cast());
    let raw_props = raw_props.cast::<RawProps>();
    let mut records = Vec::with_capacity(count as usize);
    let mut name_bytes = 0usize;
    for index in 0..count {
        let mut is_dir = PropVariant::EMPTY;
        let mut size = PropVariant::EMPTY;
        let mut encrypted = PropVariant::EMPTY;
        let mut anti = PropVariant::EMPTY;
        let mut attrib = PropVariant::EMPTY;
        let mut posix = PropVariant::EMPTY;
        let mut symlink = PropVariant::EMPTY;
        let mut alt_stream = PropVariant::EMPTY;
        for (id, prop) in [
            (KPID_IS_DIR, &mut is_dir),
            (KPID_SIZE, &mut size),
            (KPID_ENCRYPTED, &mut encrypted),
            (KPID_IS_ANTI, &mut anti),
            (KPID_ATTRIBUTES, &mut attrib),
            (KPID_POSIX_ATTRIBUTES, &mut posix),
            (KPID_SYMLINK, &mut symlink),
            (KPID_IS_ALT_STREAM, &mut alt_stream),
        ] {
            exact(unsafe { ((*(*archive).vtable).get_property)(archive, index, id, prop) })?;
        }
        let mut crc = PropVariant::EMPTY;
        if include_crc {
            exact(unsafe {
                ((*(*archive).vtable).get_property)(archive, index, KPID_CRC, &mut crc)
            })?;
        }
        let directory = is_dir.boolean().ok_or(E_FAIL)?;
        let encrypted = optional_bool(&encrypted)?;
        let anti = optional_bool(&anti)?;
        let alt_stream = optional_bool(&alt_stream)?;
        if encrypted == Some(true)
            || anti == Some(true)
            || symlink.vt != VT_EMPTY_U16
            || alt_stream == Some(true)
        {
            return Err(E_FAIL);
        }
        let attributes = optional_u32(&attrib)?;
        let posix_attributes = optional_u32(&posix)?;
        if attributes.is_some_and(|a| !valid_windows_attributes(a, directory))
            || posix_attributes.is_some_and(|a| {
                a & 0o170000 != 0 && a & 0o170000 != if directory { 0o040000 } else { 0o100000 }
            })
        {
            return Err(E_FAIL);
        }
        let mut raw_path = ptr::null();
        let mut raw_path_bytes = 0u32;
        let mut raw_path_type = 0u32;
        exact(unsafe {
            ((*(*raw_props).vtable).get_raw_prop)(
                raw_props,
                index,
                KPID_PATH,
                &mut raw_path,
                &mut raw_path_bytes,
                &mut raw_path_type,
            )
        })?;
        let components =
            unsafe { decode_raw_path(raw_path, raw_path_bytes, raw_path_type, &mut name_bytes)? };
        let declared_size = if directory {
            if size.vt != VT_EMPTY_U16
                && size.u64().or_else(|| size.u32().map(u64::from)) != Some(0)
            {
                return Err(E_FAIL);
            }
            None
        } else {
            Some(
                size.u64()
                    .or_else(|| size.u32().map(u64::from))
                    .ok_or(E_FAIL)?,
            )
        };
        let crc32 = if include_crc {
            optional_crc32(&crc)?
        } else {
            None
        };
        records.push(super::seven_zip_extract_plan::IndexedRecord {
            index,
            components,
            kind: if directory {
                NodeKind::Directory
            } else {
                NodeKind::File
            },
            declared_size,
            crc32,
        });
    }
    // 入力 callback が列挙中に失敗していたら、payload callback を始める前に止める。
    if unsafe { (*(*stream_ptr).state).failed.load(Ordering::Acquire) } {
        return Err(E_FAIL);
    }
    // 再列挙と payload 読み出しを同一 archive instance に閉じ、再 open の隙間を作らない。
    let pending = after_inventory(archive, &records)?;
    drop(raw_owner);
    drop(_close);
    drop(archive_owner);
    let source_failed = unsafe { (*(*stream_ptr).state).failed.load(Ordering::Acquire) };
    drop(stream_owner);
    drop(module);
    finalize(pending, source_failed)?;
    Ok(records)
}

fn valid_windows_attributes(attributes: u32, directory: bool) -> bool {
    if attributes & (0x40 | 0x400) != 0 {
        return false;
    }
    // Unix拡張属性では種別が上位16ビットに入るため、Windows属性だけで通常ファイルと判定しない。
    if attributes & 0x8000 != 0 {
        return match (attributes >> 16) & 0xf000 {
            0x8000 => !directory,
            0x4000 => directory,
            _ => false,
        };
    }
    true
}

struct ArchiveClose(*mut Archive);
impl Drop for ArchiveClose {
    fn drop(&mut self) {
        unsafe {
            let _ = ((*(*self.0).vtable).close)(self.0);
        }
    }
}
fn exact(hr: i32) -> Result<(), i32> {
    if hr == S_OK.0 { Ok(()) } else { Err(hr) }
}
fn numeric(value: &PropVariant) -> Option<u64> {
    value.u64().or_else(|| value.u32().map(u64::from))
}
fn optional_numeric_zero(value: &PropVariant) -> Option<u64> {
    if value.vt == VT_EMPTY_U16 {
        Some(0)
    } else {
        numeric(value)
    }
}
fn optional_bool(value: &PropVariant) -> Result<Option<bool>, i32> {
    if value.vt == VT_EMPTY_U16 {
        Ok(None)
    } else {
        value.boolean().map(Some).ok_or(E_FAIL)
    }
}
fn optional_u32(value: &PropVariant) -> Result<Option<u32>, i32> {
    if value.vt == VT_EMPTY_U16 {
        Ok(None)
    } else {
        value.u32().map(Some).ok_or(E_FAIL)
    }
}
fn optional_crc32(value: &PropVariant) -> Result<Option<u32>, i32> {
    if value.vt == VT_EMPTY_U16 {
        Ok(None)
    } else if value.vt == VT_UI4_U16 {
        value.u32().map(Some).ok_or(E_FAIL)
    } else {
        Err(E_FAIL)
    }
}
unsafe fn decode_raw_path(
    raw: *const c_void,
    byte_len: u32,
    raw_type: u32,
    total_name_bytes: &mut usize,
) -> Result<Vec<String>, i32> {
    if raw.is_null()
        || raw_type != 0x51
        || byte_len < 2
        || byte_len as usize > MAX_NAME_BYTES
        || byte_len % 2 != 0
    {
        return Err(E_FAIL);
    }
    let units = unsafe { std::slice::from_raw_parts(raw.cast::<u16>(), byte_len as usize / 2) };
    if units.last() != Some(&0) || units[..units.len() - 1].contains(&0) {
        return Err(E_FAIL);
    }
    decode_path(&units[..units.len() - 1], total_name_bytes)
}

fn decode_path(units: &[u16], total_bytes: &mut usize) -> Result<Vec<String>, i32> {
    if units.is_empty() || units.contains(&0) {
        return Err(E_FAIL);
    }
    let path = String::from_utf16(units).map_err(|_| E_FAIL)?;
    let depth = path.split('/').count();
    if depth > MAX_DEPTH {
        return Err(E_FAIL);
    }
    let mut components = Vec::with_capacity(depth);
    for part in path.split('/') {
        if part.is_empty() || part.contains('\\') || part.chars().any(char::is_control) {
            return Err(E_FAIL);
        }
        components.push(part.to_owned());
    }
    *total_bytes = total_bytes.checked_add(path.len()).ok_or(E_FAIL)?;
    if *total_bytes > MAX_NAME_BYTES {
        return Err(E_FAIL);
    }
    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn stream_seeks_within_declared_length_and_reads_exact_bytes() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abcd").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let stream = Box::new(InStream {
            vtable: &STREAM_VTABLE,
            state: Box::new(InputState {
                file,
                length: 4,
                failed: AtomicBool::new(false),
            }),
            refs: AtomicU32::new(1),
        });
        let raw = Box::into_raw(stream);
        let mut output = [0u8; 3];
        let mut read = 0;
        assert_eq!(
            unsafe { stream_read(raw, output.as_mut_ptr().cast(), 2, &mut read) },
            S_OK.0
        );
        assert_eq!((&output[..2], read), (&b"ab"[..], 2));
        let mut position = 0;
        assert_eq!(unsafe { stream_seek(raw, -1, 2, &mut position) }, S_OK.0);
        assert_eq!(position, 3);
        assert_eq!(
            unsafe { stream_read(raw, output.as_mut_ptr().cast(), 3, &mut read) },
            S_OK.0
        );
        assert_eq!((&output[..1], read), (&b"d"[..], 1));
        assert_eq!(unsafe { stream_seek(raw, 1, 0, &mut position) }, S_OK.0);
        assert_eq!(unsafe { stream_seek(raw, 5, 0, &mut position) }, E_FAIL);
        assert_eq!(
            unsafe { stream_read(raw, output.as_mut_ptr().cast(), 1, &mut read) },
            E_FAIL
        );
        unsafe {
            stream_release(raw.cast());
        }
        assert_eq!(size_of::<PropVariant>(), 24);
        assert_eq!(
            size_of::<InStreamVtable>(),
            size_of::<UnknownVtable>() + size_of::<usize>() * 2
        );
        assert_eq!(size_of::<ArchiveVtable>(), size_of::<usize>() * 9);
        assert_eq!(size_of::<RawPropsVtable>(), size_of::<usize>() * 7);
    }

    #[test]
    fn names_are_strict_and_bounded_before_component_retention() {
        let mut bytes = 0;
        let mut named = "日本語/a.txt".encode_utf16().collect::<Vec<_>>();
        named.push(0);
        let raw_len = (named.len() * 2) as u32;
        assert_eq!(
            unsafe { decode_raw_path(named.as_ptr().cast(), raw_len, 0x51, &mut bytes) }.unwrap(),
            ["日本語", "a.txt"]
        );
        let invalid_surrogate = [0xD800, 0];
        assert!(
            unsafe { decode_raw_path(invalid_surrogate.as_ptr().cast(), 4, 0x51, &mut bytes) }
                .is_err()
        );
        let embedded_zero = [b'a' as u16, 0, b'b' as u16, 0];
        assert!(
            unsafe { decode_raw_path(embedded_zero.as_ptr().cast(), 8, 0x51, &mut bytes) }.is_err()
        );
        assert!(
            unsafe { decode_raw_path(named.as_ptr().cast(), raw_len, 0x52, &mut bytes) }.is_err()
        );
        assert!(
            unsafe { decode_raw_path(named.as_ptr().cast(), raw_len - 1, 0x51, &mut bytes) }
                .is_err()
        );
        let unterminated = [b'x' as u16];
        assert!(
            unsafe { decode_raw_path(unterminated.as_ptr().cast(), 2, 0x51, &mut bytes) }.is_err()
        );
        assert!(decode_path(&"a//b".encode_utf16().collect::<Vec<_>>(), &mut bytes).is_err());
        assert!(decode_path(&"a\\b".encode_utf16().collect::<Vec<_>>(), &mut bytes).is_err());
        let too_deep = (0..=MAX_DEPTH).map(|_| "a").collect::<Vec<_>>().join("/");
        assert!(decode_path(&too_deep.encode_utf16().collect::<Vec<_>>(), &mut bytes).is_err());
        let mut exact = 0;
        assert!(
            decode_path(
                &"a".repeat(MAX_NAME_BYTES)
                    .encode_utf16()
                    .collect::<Vec<_>>(),
                &mut exact
            )
            .is_ok()
        );
        assert_eq!(exact, MAX_NAME_BYTES);
        assert!(
            decode_path(
                &"a".repeat(1).encode_utf16().collect::<Vec<_>>(),
                &mut exact
            )
            .is_err()
        );
    }

    #[test]
    fn unix_attribute_kinds_must_match_regular_file_or_directory() {
        assert!(valid_windows_attributes(0x81a4_8020, false));
        assert!(valid_windows_attributes(0x41ed_8020, true));
        for rejected in [
            (0xa1ff_8020, false), // symlink
            (0x11a4_8020, false), // fifo
            (0x21a4_8020, false), // character device
            (0x61a4_8020, false), // block device
            (0xc1a4_8020, false), // socket
            (0x41ed_8020, false), // directory-kind mismatch
            (0x81a4_8020, true),  // regular-file-kind mismatch
        ] {
            assert!(!valid_windows_attributes(rejected.0, rejected.1));
        }
    }

    #[test]
    fn optional_crc_accepts_empty_or_ui4_only() {
        let empty = PropVariant::EMPTY;
        assert_eq!(optional_crc32(&empty), Ok(None));
        let mut crc = PropVariant::EMPTY;
        crc.vt = VT_UI4_U16;
        crc.value[0] = 0;
        assert_eq!(optional_crc32(&crc), Ok(Some(0)));
        crc.vt = VT_UI8_U16;
        assert_eq!(optional_crc32(&crc), Err(E_FAIL));
    }
}
