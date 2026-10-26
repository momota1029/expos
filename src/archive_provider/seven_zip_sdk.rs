//! インストール済み 7-Zip SDK に対する、ハンドル限定の非公開アダプター。
//! SDK にはストリームと不変メタデータだけを渡し、候補出力のパスは渡さない。

#![cfg(windows)]
#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    ffi::c_void,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr,
    sync::atomic::{AtomicU32, Ordering},
    sync::{Arc, atomic::AtomicBool},
};

use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE, S_OK, SysAllocStringLen},
        Globalization::CompareStringOrdinal,
        System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            LoadLibraryExW,
        },
    },
    core::{GUID, PCWSTR},
};

const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
pub(super) const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
pub(super) const E_ABORT: i32 = 0x8000_4004u32 as i32;
pub(super) const E_FAIL: i32 = 0x8000_4005u32 as i32;
const VT_EMPTY: u16 = 0;
const VT_BSTR: u16 = 8;
const VT_BOOL: u16 = 11;
const VT_UI4: u16 = 19;
const VT_UI8: u16 = 21;
const ERROR_NEGATIVE_SEEK_HRESULT: i32 = 0x8007_0216u32 as i32;
pub(super) const MAX_ITEMS: usize = 1024;
pub(super) const MAX_NAME_UNITS: usize = 32 * 1024;
pub(super) const MAX_TOTAL_NAME_BYTES: usize = 2 * 1024 * 1024;
#[cfg(test)]
pub(super) static LAST_CREATE_PHASE: AtomicU32 = AtomicU32::new(0);
const FIXED_OUTPUT_HEADROOM: u64 = 8 * 1024 * 1024 + 32;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x0000000000000000c000000000000046);
const IID_I_PROGRESS: GUID = sdk_iid(0, 5);
const IID_I_SEQUENTIAL_IN_STREAM: GUID = sdk_iid(3, 1);
const IID_I_SEQUENTIAL_OUT_STREAM: GUID = sdk_iid(3, 2);
const IID_I_OUT_STREAM: GUID = sdk_iid(3, 4);
const IID_I_OUT_ARCHIVE: GUID = sdk_iid(6, 0xA0);
const IID_I_SET_PROPERTIES: GUID = sdk_iid(6, 3);
const IID_I_UPDATE_CALLBACK: GUID = sdk_iid(6, 0x80);
const CLSID_7Z_FORMAT: GUID = GUID::from_u128(0x23170f6940c1278a1000000110070000);

const fn sdk_iid(group: u16, interface: u8) -> GUID {
    // SDK の Guid.txt にあるグループとサフィックスをそのまま GUID に写す。
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
        vt: VT_EMPTY,
        reserved: [0; 3],
        value: [0; 2],
    };

    fn ui4(value: u32) -> Self {
        let mut result = Self::EMPTY;
        result.vt = VT_UI4;
        result.value[0] = value as u64;
        result
    }

    fn ui8(value: u64) -> Self {
        let mut result = Self::EMPTY;
        result.vt = VT_UI8;
        result.value[0] = value;
        result
    }

    fn boolean(value: bool) -> Self {
        let mut result = Self::EMPTY;
        result.vt = VT_BOOL;
        result.value[0] = if value { u16::MAX as u64 } else { 0 };
        result
    }

    fn bstr(value: &[u16]) -> Result<Self, i32> {
        let len = u32::try_from(value.len()).map_err(|_| E_INVALIDARG)?;
        let text = unsafe { SysAllocStringLen(Some(&value[..len as usize])) };
        if text.as_ptr().is_null() {
            return Err(E_FAIL);
        }
        let mut result = Self::EMPTY;
        result.vt = VT_BSTR;
        result.value[0] = text.into_raw() as usize as u64;
        Ok(result)
    }
}
impl Drop for PropVariant {
    fn drop(&mut self) {
        if self.vt == VT_BSTR && self.value[0] != 0 {
            unsafe {
                drop(windows::core::BSTR::from_raw(
                    self.value[0] as usize as *const u16,
                ));
            }
            self.value[0] = 0;
            self.vt = VT_EMPTY;
        }
    }
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
struct SeqInVtable {
    unknown: UnknownVtable,
    read: unsafe extern "system" fn(*mut SeqIn, *mut c_void, u32, *mut u32) -> i32,
}

#[repr(C)]
struct SeqIn {
    vtable: *const SeqInVtable,
    state: *mut InputState,
    refs: AtomicU32,
}

#[repr(C)]
struct OutVtable {
    unknown: UnknownVtable,
    write: unsafe extern "system" fn(*mut OutStream, *const c_void, u32, *mut u32) -> i32,
    seek: unsafe extern "system" fn(*mut OutStream, i64, u32, *mut u64) -> i32,
    set_size: unsafe extern "system" fn(*mut OutStream, u64) -> i32,
}

#[repr(C)]
struct OutStream {
    vtable: *const OutVtable,
    state: *mut OutputState,
    refs: AtomicU32,
}

#[repr(C)]
struct UpdateVtable {
    unknown: UnknownVtable,
    set_total: unsafe extern "system" fn(*mut UpdateCallback, u64) -> i32,
    set_completed: unsafe extern "system" fn(*mut UpdateCallback, *const u64) -> i32,
    get_update_item_info:
        unsafe extern "system" fn(*mut UpdateCallback, u32, *mut i32, *mut i32, *mut u32) -> i32,
    get_property: unsafe extern "system" fn(*mut UpdateCallback, u32, u32, *mut PropVariant) -> i32,
    get_stream: unsafe extern "system" fn(*mut UpdateCallback, u32, *mut *mut SeqIn) -> i32,
    set_operation_result: unsafe extern "system" fn(*mut UpdateCallback, i32) -> i32,
}

#[repr(C)]
struct UpdateCallback {
    vtable: *const UpdateVtable,
    state: *mut UpdateState,
    refs: AtomicU32,
}

#[repr(C)]
struct OutArchiveVtable {
    unknown: UnknownVtable,
    update_items:
        unsafe extern "system" fn(*mut OutArchive, *mut OutStream, u32, *mut UpdateCallback) -> i32,
    get_file_time_type: unsafe extern "system" fn(*mut OutArchive, *mut u32) -> i32,
}
#[repr(C)]
struct OutArchive {
    vtable: *const OutArchiveVtable,
}

#[repr(C)]
struct SetPropertiesVtable {
    unknown: UnknownVtable,
    set_properties: unsafe extern "system" fn(
        *mut SetProperties,
        *const *const u16,
        *const PropVariant,
        u32,
    ) -> i32,
}
#[repr(C)]
struct SetProperties {
    vtable: *const SetPropertiesVtable,
}

#[derive(Debug)]
pub(super) struct Item {
    /// 先頭スラッシュのない、`/` 区切りのアーカイブ相対 Unicode パス。
    pub name: Vec<u16>,
    pub directory: bool,
    pub length: u64,
    /// ファイルには継承した読み取り専用ハンドルを一つだけ持たせる。
    pub source: Option<File>,
}

struct InputState {
    file: File,
    remaining: u64,
    expected: u64,
    failed: bool,
    errors: Arc<AtomicBool>,
}
struct OutputState {
    file: File,
    failed: bool,
    max_size: u64,
    errors: Arc<AtomicBool>,
}
struct UpdateState {
    items: Vec<Item>,
    streams: Vec<Option<Box<InputState>>>,
    requested: Vec<bool>,
    results: Vec<bool>,
    cancelled: *const AtomicBool,
    failed: bool,
    errors: Arc<AtomicBool>,
}

struct SdkModule(HMODULE);
impl Drop for SdkModule {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
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

type CreateObjectFn = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;

/// 検証済み DLL のリースを持つ provider 子プロセス内でだけ呼び出す。
pub(super) fn create_from_handles(
    dll: &Path,
    output: File,
    items: Vec<Item>,
    cancelled: &AtomicBool,
) -> Result<(), i32> {
    create_with_profile_for_child(dll, output, items, cancelled, false)
}

#[cfg(test)]
pub(super) fn create_with_unsupported_test_profile(
    dll: &Path,
    output: File,
    items: Vec<Item>,
    cancelled: &AtomicBool,
) -> Result<(), i32> {
    create_with_profile_for_child(dll, output, items, cancelled, true)
}

fn create_with_profile_for_child(
    dll: &Path,
    output: File,
    items: Vec<Item>,
    cancelled: &AtomicBool,
    unsupported_profile: bool,
) -> Result<(), i32> {
    #[cfg(test)]
    LAST_CREATE_PHASE.store(1, Ordering::Release);
    validate_items(&items)?;
    if !dll.is_absolute() || dll.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(E_INVALIDARG);
    }
    let errors = Arc::new(AtomicBool::new(false));
    let dll_wide: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
    #[cfg(test)]
    LAST_CREATE_PHASE.store(2, Ordering::Release);
    let module = unsafe {
        LoadLibraryExW(
            PCWSTR(dll_wide.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map(SdkModule)
    .map_err(|_| E_FAIL)?;
    let proc =
        unsafe { GetProcAddress(module.0, windows::core::s!("CreateObject")) }.ok_or(E_FAIL)?;
    let create: CreateObjectFn = unsafe { std::mem::transmute(proc) };
    let mut raw = ptr::null_mut();
    #[cfg(test)]
    LAST_CREATE_PHASE.store(3, Ordering::Release);
    let hr = unsafe { create(&CLSID_7Z_FORMAT, &IID_I_OUT_ARCHIVE, &mut raw) };
    check_exact_success(hr)?;
    let archive = raw.cast::<OutArchive>();
    if archive.is_null() {
        return Err(E_FAIL);
    }
    let archive_owner = ComPtr(archive.cast());
    let mut set_raw = ptr::null_mut();
    #[cfg(test)]
    LAST_CREATE_PHASE.store(4, Ordering::Release);
    let hr = unsafe {
        ((*(*archive).vtable).unknown.query_interface)(
            archive.cast(),
            &IID_I_SET_PROPERTIES,
            &mut set_raw,
        )
    };
    if hr != S_OK.0 {
        return Err(if hr < 0 { hr } else { E_FAIL });
    }
    if set_raw.is_null() {
        return Err(E_NOINTERFACE);
    }
    let set_properties = set_raw.cast::<SetProperties>();
    let set_owner = ComPtr(set_properties.cast());
    #[cfg(test)]
    LAST_CREATE_PHASE.store(5, Ordering::Release);
    let mut properties = fixed_properties()?;
    if unsupported_profile {
        properties[4].1 =
            PropVariant::bstr(&"not-a-thread-count".encode_utf16().collect::<Vec<_>>())?;
    }
    let names: Vec<Vec<u16>> = properties.iter().map(|(name, _)| name.clone()).collect();
    let name_ptrs: Vec<*const u16> = names.iter().map(|name| name.as_ptr()).collect();
    let values: Vec<PropVariant> = properties.into_iter().map(|(_, value)| value).collect();
    #[cfg(test)]
    LAST_CREATE_PHASE.store(9, Ordering::Release);
    let prop_hr = unsafe {
        ((*(*set_properties).vtable).set_properties)(
            set_properties,
            name_ptrs.as_ptr(),
            values.as_ptr(),
            values.len() as u32,
        )
    };
    drop(set_owner);
    if prop_hr != S_OK.0 {
        #[cfg(test)]
        LAST_CREATE_PHASE.store(10, Ordering::Release);
        return Err(if prop_hr < 0 { prop_hr } else { E_FAIL });
    }

    let max_size = items
        .iter()
        .try_fold(0u64, |sum, item| sum.checked_add(item.length))
        .and_then(|payload| payload.checked_mul(2))
        .and_then(|payload| payload.checked_add(FIXED_OUTPUT_HEADROOM))
        .ok_or(E_INVALIDARG)?;
    let output_file = output;
    if !output_file.metadata().map_err(|_| E_FAIL)?.is_file() {
        return Err(E_INVALIDARG);
    }
    let output_state = Box::new(OutputState {
        file: output_file,
        failed: false,
        max_size,
        errors: errors.clone(),
    });
    let mut output_state = output_state;
    let out = OutStream {
        vtable: &OUT_VTABLE,
        state: &mut *output_state,
        refs: AtomicU32::new(1),
    };
    #[cfg(test)]
    LAST_CREATE_PHASE.store(6, Ordering::Release);
    let mut streams = Vec::with_capacity(items.len());
    for item in &items {
        if item.directory {
            streams.push(None);
            continue;
        }
        let file = prepare_source(item)?;
        streams.push(Some(Box::new(InputState {
            file,
            remaining: item.length,
            expected: item.length,
            failed: false,
            errors: errors.clone(),
        })));
    }
    let mut state = Box::new(UpdateState {
        requested: vec![false; items.len()],
        items,
        streams,
        results: Vec::new(),
        cancelled: cancelled as *const AtomicBool,
        failed: false,
        errors: errors.clone(),
    });
    let mut callback = UpdateCallback {
        vtable: &UPDATE_VTABLE,
        state: &mut *state,
        refs: AtomicU32::new(1),
    };
    #[cfg(test)]
    LAST_CREATE_PHASE.store(7, Ordering::Release);
    let hr = unsafe {
        ((*(*archive).vtable).update_items)(
            archive,
            (&out as *const OutStream).cast_mut(),
            state.items.len() as u32,
            &mut callback,
        )
    };
    let requested_files = state
        .requested
        .iter()
        .zip(&state.items)
        .filter(|(requested, item)| !item.directory && **requested)
        .count();
    let callback_failed = state.failed
        || errors.load(Ordering::Acquire)
        || state
            .items
            .iter()
            .enumerate()
            .any(|(i, item)| !item.directory && item.length != 0 && !state.requested[i])
        || state.results.len() != requested_files
        || state.results.iter().any(|ok| !ok)
        || state
            .requested
            .iter()
            .zip(&state.items)
            .any(|(requested, item)| !item.directory && item.length != 0 && !requested)
        || output_state.failed
        || state
            .streams
            .iter()
            .flatten()
            .any(|s| s.failed || s.remaining != 0);
    drop(archive_owner);
    finish_update(hr, callback_failed, cancelled)
}

fn finish_update(hr: i32, callback_failed: bool, cancelled: &AtomicBool) -> Result<(), i32> {
    if hr != S_OK.0 {
        return Err(if hr < 0 { hr } else { E_FAIL });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(E_ABORT);
    }
    if callback_failed {
        return Err(E_FAIL);
    }
    Ok(())
}

fn prepare_source(item: &Item) -> Result<File, i32> {
    let file = item
        .source
        .as_ref()
        .ok_or(E_INVALIDARG)?
        .try_clone()
        .map_err(|_| E_FAIL)?;
    let metadata = file.metadata().map_err(|_| E_FAIL)?;
    validate_source_metadata(metadata.is_file(), metadata.len(), item.length)?;
    let mut file = file;
    file.seek(SeekFrom::Start(0)).map_err(|_| E_FAIL)?;
    Ok(file)
}

fn validate_source_metadata(is_file: bool, length: u64, expected: u64) -> Result<(), i32> {
    if is_file && length == expected {
        Ok(())
    } else {
        Err(E_INVALIDARG)
    }
}

fn check_exact_success(hr: i32) -> Result<(), i32> {
    if hr == S_OK.0 {
        Ok(())
    } else {
        Err(if hr < 0 { hr } else { E_FAIL })
    }
}

fn validate_items(items: &[Item]) -> Result<(), i32> {
    validate_metadata(items.iter().map(|item| ItemMetadata {
        name: &item.name,
        directory: item.directory,
        length: item.length,
        has_source: item.source.is_some(),
    }))
}

/// SDK 呼出し前とハンドル受入れ前で、同じ名前・source 関連付け規則を使う。
#[derive(Clone, Copy, Debug)]
pub(super) struct ItemMetadata<'a> {
    pub name: &'a [u16],
    pub directory: bool,
    pub length: u64,
    pub has_source: bool,
}

pub(super) fn validate_metadata<'a>(
    items: impl IntoIterator<Item = ItemMetadata<'a>>,
) -> Result<(), i32> {
    let items = items.into_iter().collect::<Vec<_>>();
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(E_INVALIDARG);
    }
    let mut total_names = 0usize;
    let mut seen = Vec::<(Vec<Vec<u16>>, bool)>::new();
    for ItemMetadata {
        name,
        directory,
        length,
        has_source,
    } in items
    {
        if name.is_empty()
            || name.len() > MAX_NAME_UNITS
            || name[0] == b'/' as u16
            || name.last() == Some(&(b'/' as u16))
        {
            return Err(E_INVALIDARG);
        }
        if name
            .iter()
            .any(|&u| u == 0 || u < 0x20 || u == b'\\' as u16 || u == b':' as u16)
        {
            return Err(E_INVALIDARG);
        }
        let text = String::from_utf16(name).map_err(|_| E_INVALIDARG)?;
        let components = text.split('/').collect::<Vec<_>>();
        if components.len() > 128
            || components
                .iter()
                .any(|component| !valid_component(component))
        {
            return Err(E_INVALIDARG);
        }
        total_names = total_names.checked_add(text.len()).ok_or(E_INVALIDARG)?;
        if total_names > MAX_TOTAL_NAME_BYTES {
            return Err(E_INVALIDARG);
        }
        let components = components
            .into_iter()
            .map(|component| component.encode_utf16().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        for (prior, prior_directory) in &seen {
            let shared = prior.len().min(components.len());
            if (0..shared).all(|i| ordinal_eq(&prior[i], &components[i])) {
                if prior.len() == components.len()
                    || (prior.len() < components.len() && !prior_directory)
                {
                    return Err(E_INVALIDARG);
                }
                if components.len() < prior.len() && !directory {
                    return Err(E_INVALIDARG);
                }
            }
        }
        seen.push((components, directory));
        if directory {
            if has_source || length != 0 {
                return Err(E_INVALIDARG);
            }
        } else if !has_source {
            return Err(E_INVALIDARG);
        }
    }
    Ok(())
}

fn ordinal_eq(left: &[u16], right: &[u16]) -> bool {
    unsafe { CompareStringOrdinal(left, right, true).0 == 2 }
}

fn valid_component(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with([' ', '.'])
        || component
            .chars()
            .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c <= '\u{1f}')
    {
        return false;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(suffix) = upper.strip_prefix(prefix) {
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }
    }
    if ["COM", "LPT"].iter().any(|prefix| {
        ["¹", "²", "³"]
            .iter()
            .any(|digit| stem == format!("{prefix}{digit}"))
    }) {
        return false;
    }
    true
}

unsafe fn cancelled(state: &UpdateState) -> bool {
    !state.cancelled.is_null() && (*state.cancelled).load(Ordering::Acquire)
}

fn fixed_properties() -> Result<Vec<(Vec<u16>, PropVariant)>, i32> {
    // 値の型と設定名は 24.09 の ISetProperties 契約に固定し、受理されない DLL は失敗させる。
    let bstr = |s: &str| PropVariant::bstr(&s.encode_utf16().collect::<Vec<_>>());
    Ok(vec![
        ("0".encode_utf16().chain([0]).collect(), bstr("Deflate")?),
        (
            "s".encode_utf16().chain([0]).collect(),
            PropVariant::boolean(false),
        ),
        (
            "hc".encode_utf16().chain([0]).collect(),
            PropVariant::boolean(false),
        ),
        (
            "f".encode_utf16().chain([0]).collect(),
            PropVariant::boolean(false),
        ),
        (
            "mt".encode_utf16().chain([0]).collect(),
            PropVariant::ui4(1),
        ),
    ])
}

unsafe extern "system" fn unknown_query(
    this: *mut Unknown,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    let target = if *iid == IID_IUNKNOWN {
        Some(this.cast::<c_void>())
    } else if *iid == IID_I_SEQUENTIAL_IN_STREAM {
        Some(this.cast::<c_void>())
    } else {
        None
    };
    if let Some(p) = target {
        *out = p;
        unknown_add_ref(this);
        S_OK.0
    } else {
        E_NOINTERFACE
    }
}
unsafe extern "system" fn out_query(
    this: *mut Unknown,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    if *iid == IID_IUNKNOWN || *iid == IID_I_SEQUENTIAL_OUT_STREAM || *iid == IID_I_OUT_STREAM {
        *out = this.cast();
        unknown_add_ref(this);
        S_OK.0
    } else {
        E_NOINTERFACE
    }
}
unsafe extern "system" fn update_query(
    this: *mut Unknown,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    if *iid == IID_IUNKNOWN || *iid == IID_I_PROGRESS || *iid == IID_I_UPDATE_CALLBACK {
        *out = this.cast();
        unknown_add_ref(this);
        S_OK.0
    } else {
        E_NOINTERFACE
    }
}
unsafe extern "system" fn unknown_add_ref(this: *mut Unknown) -> u32 {
    let refs = &*((this as *mut *const c_void).add(2).cast::<AtomicU32>());
    refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn unknown_release(this: *mut Unknown) -> u32 {
    let refs = &*((this as *mut *const c_void).add(2).cast::<AtomicU32>());
    refs.fetch_sub(1, Ordering::Release).saturating_sub(1)
}
unsafe extern "system" fn input_add_ref(this: *mut Unknown) -> u32 {
    let stream = this.cast::<SeqIn>();
    (*stream).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn input_release(this: *mut Unknown) -> u32 {
    let stream = this.cast::<SeqIn>();
    let remaining = (*stream)
        .refs
        .fetch_sub(1, Ordering::Release)
        .saturating_sub(1);
    if remaining == 0 {
        drop(Box::from_raw(stream));
    }
    remaining
}
unsafe extern "system" fn input_read(
    this: *mut SeqIn,
    data: *mut c_void,
    size: u32,
    processed: *mut u32,
) -> i32 {
    if this.is_null() || processed.is_null() || (data.is_null() && size != 0) {
        return E_POINTER;
    }
    *processed = 0;
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if size == 0 {
        return S_OK.0;
    }
    if size == 0 || state.remaining == 0 {
        return S_OK.0;
    }
    let slice = std::slice::from_raw_parts_mut(data.cast::<u8>(), size as usize);
    let cap = slice.len().min(state.remaining as usize);
    match state.file.read(&mut slice[..cap]) {
        Ok(0) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
        Ok(n) => {
            state.remaining -= n as u64;
            *processed = n as u32;
            S_OK.0
        }
        Err(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
    }
}
unsafe extern "system" fn output_write(
    this: *mut OutStream,
    data: *const c_void,
    size: u32,
    processed: *mut u32,
) -> i32 {
    if this.is_null() || processed.is_null() || (data.is_null() && size != 0) {
        return E_POINTER;
    }
    *processed = 0;
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if size == 0 {
        return S_OK.0;
    }
    let position = match state.file.stream_position() {
        Ok(position) => position,
        Err(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            return E_FAIL;
        }
    };
    if position
        .checked_add(size as u64)
        .is_none_or(|end| end > state.max_size)
    {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_FAIL;
    }
    let slice = std::slice::from_raw_parts(data.cast::<u8>(), size as usize);
    match state.file.write(slice) {
        Ok(n) if n == size as usize => {
            *processed = n as u32;
            S_OK.0
        }
        Ok(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
        Err(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
    }
}
unsafe extern "system" fn output_seek(
    this: *mut OutStream,
    offset: i64,
    origin: u32,
    position: *mut u64,
) -> i32 {
    if this.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if origin > 2 {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_INVALIDARG;
    }
    let from = match origin {
        0 => SeekFrom::Start(offset.max(0) as u64),
        1 => SeekFrom::Current(offset),
        _ => SeekFrom::End(offset),
    };
    if origin == 0 && offset < 0 {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return ERROR_NEGATIVE_SEEK_HRESULT;
    }
    match state.file.seek(from) {
        Ok(pos) if pos <= state.max_size => {
            if !position.is_null() {
                *position = pos;
            }
            S_OK.0
        }
        Ok(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
        Err(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
    }
}
unsafe extern "system" fn output_set_size(this: *mut OutStream, size: u64) -> i32 {
    if this.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if size > state.max_size {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_FAIL;
    }
    match state.file.set_len(size) {
        Ok(()) => S_OK.0,
        Err(_) => {
            state.failed = true;
            state.errors.store(true, Ordering::Release);
            E_FAIL
        }
    }
}
unsafe extern "system" fn progress_total(this: *mut UpdateCallback, _: u64) -> i32 {
    if this.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if cancelled(state) {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        E_ABORT
    } else {
        S_OK.0
    }
}
unsafe extern "system" fn progress_completed(this: *mut UpdateCallback, _: *const u64) -> i32 {
    if this.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if cancelled(state) {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        E_ABORT
    } else {
        S_OK.0
    }
}
unsafe extern "system" fn update_info(
    this: *mut UpdateCallback,
    index: u32,
    new_data: *mut i32,
    new_props: *mut i32,
    archive_index: *mut u32,
) -> i32 {
    if this.is_null() || new_data.is_null() || new_props.is_null() || archive_index.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    if index as usize >= state.items.len() {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_INVALIDARG;
    }
    *new_data = 1;
    *new_props = 1;
    *archive_index = u32::MAX;
    S_OK.0
}
unsafe extern "system" fn update_property(
    this: *mut UpdateCallback,
    index: u32,
    prop: u32,
    value: *mut PropVariant,
) -> i32 {
    if this.is_null() || value.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    let Some(item) = state.items.get(index as usize) else {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_INVALIDARG;
    };
    *value = PropVariant::EMPTY;
    match prop {
        3 => match PropVariant::bstr(&item.name) {
            Ok(v) => *value = v,
            Err(e) => {
                state.failed = true;
                state.errors.store(true, Ordering::Release);
                return e;
            }
        },
        6 => *value = PropVariant::boolean(item.directory),
        7 if !item.directory => *value = PropVariant::ui8(item.length),
        9 => *value = PropVariant::ui4(if item.directory { 0x10 } else { 0x80 }),
        _ => {}
    }
    S_OK.0
}
unsafe extern "system" fn update_get_stream(
    this: *mut UpdateCallback,
    index: u32,
    stream: *mut *mut SeqIn,
) -> i32 {
    if this.is_null() || stream.is_null() {
        return E_POINTER;
    }
    *stream = ptr::null_mut();
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    let Some(item) = state.items.get(index as usize) else {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_INVALIDARG;
    };
    if item.directory {
        return S_OK.0;
    }
    // SDK が同じ項目のストリームを再要求した場合は入力を取り違えないよう失敗させる。
    let Some(item_index) = usize::try_from(index)
        .ok()
        .filter(|&i| i < state.items.len())
    else {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_INVALIDARG;
    };
    if state.requested[item_index] {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_FAIL;
    }
    let Some(Some(source)) = state.streams.get_mut(item_index) else {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_FAIL;
    };
    if source.expected != item.length || source.remaining != item.length {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        return E_FAIL;
    }
    let input = Box::into_raw(Box::new(SeqIn {
        vtable: &SEQ_IN_VTABLE,
        state: &mut **source,
        refs: AtomicU32::new(1),
    }));
    state.requested[item_index] = true;
    *stream = input;
    S_OK.0
}
unsafe extern "system" fn update_result(this: *mut UpdateCallback, result: i32) -> i32 {
    if this.is_null() {
        return E_POINTER;
    }
    let state = &mut *(*this).state;
    if state.failed || state.errors.load(Ordering::Acquire) {
        return E_FAIL;
    }
    state.results.push(result == 0);
    if result != 0 {
        state.failed = true;
        state.errors.store(true, Ordering::Release);
        E_FAIL
    } else {
        S_OK.0
    }
}

static SEQ_IN_VTABLE: SeqInVtable = SeqInVtable {
    unknown: UnknownVtable {
        query_interface: unknown_query,
        add_ref: input_add_ref,
        release: input_release,
    },
    read: input_read,
};
static OUT_VTABLE: OutVtable = OutVtable {
    unknown: UnknownVtable {
        query_interface: out_query,
        add_ref: unknown_add_ref,
        release: unknown_release,
    },
    write: output_write,
    seek: output_seek,
    set_size: output_set_size,
};
static UPDATE_VTABLE: UpdateVtable = UpdateVtable {
    unknown: UnknownVtable {
        query_interface: update_query,
        add_ref: unknown_add_ref,
        release: unknown_release,
    },
    set_total: progress_total,
    set_completed: progress_completed,
    get_update_item_info: update_info,
    get_property: update_property,
    get_stream: update_get_stream,
    set_operation_result: update_result,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_interface_guids_match_the_7zip_sdk_layout() {
        assert_eq!(CLSID_7Z_FORMAT.data4, [0x10, 0, 0, 1, 0x10, 7, 0, 0]);
        assert_eq!(IID_I_OUT_ARCHIVE.data4, [0, 0, 0, 6, 0, 0xA0, 0, 0]);
        assert_eq!(IID_I_UPDATE_CALLBACK.data4, [0, 0, 0, 6, 0, 0x80, 0, 0]);
        assert_eq!(IID_I_PROGRESS.data4, [0, 0, 0, 0, 0, 5, 0, 0]);
        assert_eq!(IID_I_SEQUENTIAL_IN_STREAM.data4, [0, 0, 0, 3, 0, 1, 0, 0]);
        assert_eq!(IID_I_SEQUENTIAL_OUT_STREAM.data4, [0, 0, 0, 3, 0, 2, 0, 0]);
        assert_eq!(IID_I_OUT_STREAM.data4, [0, 0, 0, 3, 0, 4, 0, 0]);
        assert_eq!(IID_I_SET_PROPERTIES.data4, [0, 0, 0, 6, 0, 3, 0, 0]);
        assert_eq!(size_of::<PropVariant>(), 24);
    }

    #[test]
    fn fixed_profile_uses_only_the_verified_2409_property_types() {
        let properties = fixed_properties().unwrap();
        let names = properties
            .iter()
            .map(|(name, _)| String::from_utf16(&name[..name.len() - 1]).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["0", "s", "hc", "f", "mt"]);
        assert_eq!(
            properties
                .iter()
                .map(|(_, value)| value.vt)
                .collect::<Vec<_>>(),
            [VT_BSTR, VT_BOOL, VT_BOOL, VT_BOOL, VT_UI4]
        );
        let method = unsafe {
            std::slice::from_raw_parts(properties[0].1.value[0] as usize as *const u16, 7)
        };
        assert_eq!(String::from_utf16(method).unwrap(), "Deflate");
        for (_, value) in properties.iter().skip(1).take(3) {
            assert_eq!(value.value[0] as u16, 0);
        }
        assert_eq!(properties[4].1.value[0], 1);
    }

    #[test]
    fn update_properties_emit_fixed_directory_and_file_attributes() {
        let cancelled = AtomicBool::new(false);
        let errors = Arc::new(AtomicBool::new(false));
        let mut state = Box::new(UpdateState {
            items: vec![
                Item {
                    name: "empty-dir".encode_utf16().collect(),
                    directory: true,
                    length: 0,
                    source: None,
                },
                Item {
                    name: "empty.bin".encode_utf16().collect(),
                    directory: false,
                    length: 0,
                    source: Some(tempfile::tempfile().unwrap()),
                },
            ],
            streams: vec![None, None],
            requested: vec![false, false],
            results: Vec::new(),
            cancelled: &cancelled,
            failed: false,
            errors,
        });
        let mut callback = UpdateCallback {
            vtable: &UPDATE_VTABLE,
            state: &mut *state,
            refs: AtomicU32::new(1),
        };
        let mut directory = PropVariant::EMPTY;
        let mut file = PropVariant::EMPTY;
        assert_eq!(
            unsafe { update_property(&mut callback, 0, 9, &mut directory) },
            S_OK.0
        );
        assert_eq!(
            unsafe { update_property(&mut callback, 1, 9, &mut file) },
            S_OK.0
        );
        assert_eq!(directory.vt, VT_UI4);
        assert_eq!(directory.value[0], 0x10);
        assert_eq!(file.vt, VT_UI4);
        assert_eq!(file.value[0], 0x80);
    }

    #[test]
    fn source_preparation_rejects_directories_and_resets_eof_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let metadata = std::fs::metadata(directory.path()).unwrap();
        assert_eq!(
            validate_source_metadata(metadata.is_file(), metadata.len(), 0),
            Err(E_INVALIDARG)
        );

        let mut source = tempfile::tempfile().unwrap();
        source.write_all(b"source bytes").unwrap();
        source.seek(SeekFrom::End(0)).unwrap();
        let mut prepared = prepare_source(&Item {
            name: "source.bin".encode_utf16().collect(),
            directory: false,
            length: 12,
            source: Some(source),
        })
        .unwrap();
        let mut bytes = Vec::new();
        prepared.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"source bytes");
        assert_eq!(validate_source_metadata(true, 11, 12), Err(E_INVALIDARG));
    }

    #[test]
    fn final_cancellation_after_update_items_cannot_be_accepted() {
        let cancelled = AtomicBool::new(false);
        let state = Box::new(UpdateState {
            items: vec![Item {
                name: "directory".encode_utf16().collect(),
                directory: true,
                length: 0,
                source: None,
            }],
            streams: vec![None],
            requested: vec![false],
            results: Vec::new(),
            cancelled: &cancelled,
            failed: false,
            errors: Arc::new(AtomicBool::new(false)),
        });
        let mut callback = UpdateCallback {
            vtable: &UPDATE_VTABLE,
            state: Box::into_raw(state),
            refs: AtomicU32::new(1),
        };
        assert_eq!(
            unsafe { progress_completed(&mut callback, ptr::null()) },
            S_OK.0
        );
        cancelled.store(true, Ordering::Release);
        assert_eq!(finish_update(S_OK.0, false, &cancelled), Err(E_ABORT));
        unsafe {
            drop(Box::from_raw(callback.state));
        }
    }

    #[test]
    fn invalid_seek_is_sticky_across_later_callback() {
        for (offset, origin, expected) in
            [(0, 3, E_INVALIDARG), (-1, 0, ERROR_NEGATIVE_SEEK_HRESULT)]
        {
            let errors = Arc::new(AtomicBool::new(false));
            let mut output_state = Box::new(OutputState {
                file: tempfile::tempfile().unwrap(),
                failed: false,
                max_size: 1024,
                errors: errors.clone(),
            });
            let mut output = OutStream {
                vtable: &OUT_VTABLE,
                state: &mut *output_state,
                refs: AtomicU32::new(1),
            };
            assert_eq!(
                unsafe { output_seek(&mut output, offset, origin, ptr::null_mut()) },
                expected
            );
            assert!(errors.load(Ordering::Acquire));

            let cancelled = AtomicBool::new(false);
            let mut update_state = Box::new(UpdateState {
                items: vec![Item {
                    name: "directory".encode_utf16().collect(),
                    directory: true,
                    length: 0,
                    source: None,
                }],
                streams: vec![None],
                requested: vec![false],
                results: Vec::new(),
                cancelled: &cancelled,
                failed: false,
                errors: errors.clone(),
            });
            let mut callback = UpdateCallback {
                vtable: &UPDATE_VTABLE,
                state: &mut *update_state,
                refs: AtomicU32::new(1),
            };
            assert_eq!(unsafe { update_result(&mut callback, 0) }, E_FAIL);
            assert!(update_state.results.is_empty());
        }
    }

    #[test]
    fn stream_manifest_rejects_unsafe_or_duplicate_paths() {
        let entry = |name: &str| Item {
            name: name.encode_utf16().collect(),
            directory: false,
            length: 0,
            source: Some(tempfile::tempfile().unwrap()),
        };
        assert!(validate_items(&[entry("good/name.txt")]).is_ok());
        assert!(validate_items(&[entry("../escape")]).is_err());
        assert!(validate_items(&[entry("C:/absolute")]).is_err());
        assert!(validate_items(&[entry("x"), entry("X")]).is_err());
        assert!(validate_items(&[entry("CON.txt")]).is_err());
        assert!(validate_items(&[entry("folder/name.")]).is_err());
        assert!(validate_items(&[entry(&format!("{}x", "a/".repeat(128)))]).is_err());
        assert!(validate_items(&[entry("parent"), entry("parent/child")]).is_err());
        assert!(validate_items(&[entry("parent/child"), entry("parent")]).is_err());
        assert!(
            validate_items(&[
                Item {
                    directory: true,
                    source: None,
                    length: 0,
                    ..entry("Parent")
                },
                entry("parent/child")
            ])
            .is_ok()
        );
    }

    #[test]
    fn output_stream_abi_writes_seeks_and_truncates_exactly() {
        let file = tempfile::tempfile().unwrap();
        let mut state = Box::new(OutputState {
            file,
            failed: false,
            max_size: 1024,
            errors: Arc::new(AtomicBool::new(false)),
        });
        let mut stream = OutStream {
            vtable: &OUT_VTABLE,
            state: &mut *state,
            refs: AtomicU32::new(1),
        };
        let bytes = b"archive bytes";
        let mut processed = 0;
        assert_eq!(
            unsafe {
                output_write(
                    &mut stream,
                    bytes.as_ptr().cast(),
                    bytes.len() as u32,
                    &mut processed,
                )
            },
            S_OK.0
        );
        assert_eq!(processed, bytes.len() as u32);
        let mut position = 0;
        assert_eq!(
            unsafe { output_seek(&mut stream, 0, 0, &mut position) },
            S_OK.0
        );
        assert_eq!(position, 0);
        assert_eq!(
            unsafe { output_seek(&mut stream, 0, 0, ptr::null_mut()) },
            S_OK.0
        );
        let replacement = b"7z";
        assert_eq!(
            unsafe { output_write(&mut stream, replacement.as_ptr().cast(), 2, &mut processed) },
            S_OK.0
        );
        assert_eq!(unsafe { output_set_size(&mut stream, 2) }, S_OK.0);
        assert_eq!(
            unsafe { output_write(&mut stream, ptr::null(), 0, &mut processed) },
            S_OK.0
        );
        assert_eq!(
            unsafe { output_seek(&mut stream, 1024, 0, &mut position) },
            S_OK.0
        );
        assert_eq!(
            unsafe { output_write(&mut stream, bytes.as_ptr().cast(), 1, &mut processed) },
            E_FAIL
        );
        assert_eq!(unsafe { output_set_size(&mut stream, 1025) }, E_FAIL);
        state.file.seek(SeekFrom::Start(0)).unwrap();
        let mut actual = Vec::new();
        state.file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, replacement);
    }

    #[test]
    fn input_stream_abi_reports_exact_eof_and_rejects_early_eof() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"input").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut state = Box::new(InputState {
            file,
            remaining: 5,
            expected: 5,
            failed: false,
            errors: Arc::new(AtomicBool::new(false)),
        });
        let mut stream = SeqIn {
            vtable: &SEQ_IN_VTABLE,
            state: &mut *state,
            refs: AtomicU32::new(1),
        };
        let mut buffer = [0u8; 8];
        let mut processed = 0;
        assert_eq!(
            unsafe { input_read(&mut stream, buffer.as_mut_ptr().cast(), 8, &mut processed) },
            S_OK.0
        );
        assert_eq!(processed, 5);
        assert_eq!(&buffer[..5], b"input");
        assert_eq!(
            unsafe { input_read(&mut stream, buffer.as_mut_ptr().cast(), 1, &mut processed) },
            S_OK.0
        );
        assert_eq!(processed, 0);

        state.file.set_len(2).unwrap();
        state.file.seek(SeekFrom::Start(0)).unwrap();
        state.remaining = 5;
        state.failed = false;
        state.errors.store(false, Ordering::Release);
        assert_eq!(
            unsafe { input_read(&mut stream, buffer.as_mut_ptr().cast(), 5, &mut processed) },
            S_OK.0
        );
        assert_eq!(processed, 2);
        assert_eq!(
            unsafe { input_read(&mut stream, buffer.as_mut_ptr().cast(), 3, &mut processed) },
            E_FAIL
        );
        assert!(state.failed);
    }

    #[test]
    fn progress_callback_cancellation_and_result_failure_are_propagated() {
        let cancelled = AtomicBool::new(true);
        let mut state = Box::new(UpdateState {
            items: vec![Item {
                name: "one".encode_utf16().collect(),
                directory: true,
                length: 0,
                source: None,
            }],
            streams: vec![None],
            requested: vec![false],
            results: Vec::new(),
            cancelled: &cancelled,
            failed: false,
            errors: Arc::new(AtomicBool::new(false)),
        });
        let mut callback = UpdateCallback {
            vtable: &UPDATE_VTABLE,
            state: &mut *state,
            refs: AtomicU32::new(1),
        };
        assert_eq!(
            unsafe { progress_completed(&mut callback, ptr::null()) },
            E_ABORT
        );
        cancelled.store(false, Ordering::Release);
        assert_eq!(
            unsafe { progress_completed(&mut callback, ptr::null()) },
            E_FAIL
        );
        assert_eq!(unsafe { update_result(&mut callback, 1) }, E_FAIL);
        assert!(state.results.is_empty());
        assert!(state.errors.load(Ordering::Acquire));
    }

    #[test]
    fn callbacks_reject_repeated_or_out_of_range_stream_requests() {
        let mut file_a = tempfile::tempfile().unwrap();
        let mut file_b = tempfile::tempfile().unwrap();
        file_a.write_all(b"a").unwrap();
        file_b.write_all(b"b").unwrap();
        file_a.seek(SeekFrom::Start(0)).unwrap();
        file_b.seek(SeekFrom::Start(0)).unwrap();
        let items = vec![
            Item {
                name: "a".encode_utf16().collect(),
                directory: false,
                length: 1,
                source: Some(file_a.try_clone().unwrap()),
            },
            Item {
                name: "b".encode_utf16().collect(),
                directory: false,
                length: 1,
                source: Some(file_b.try_clone().unwrap()),
            },
        ];
        let streams = [file_a, file_b]
            .into_iter()
            .map(|file| {
                Some(Box::new(InputState {
                    file,
                    remaining: 1,
                    expected: 1,
                    failed: false,
                    errors: Arc::new(AtomicBool::new(false)),
                }))
            })
            .collect();
        let mut state = Box::new(UpdateState {
            requested: vec![false; 2],
            items,
            streams,
            results: Vec::new(),
            cancelled: ptr::null(),
            failed: false,
            errors: Arc::new(AtomicBool::new(false)),
        });
        let mut callback = UpdateCallback {
            vtable: &UPDATE_VTABLE,
            state: &mut *state,
            refs: AtomicU32::new(1),
        };
        let mut stream = ptr::null_mut();
        assert_eq!(
            unsafe { update_get_stream(&mut callback, 1, &mut stream) },
            S_OK.0
        );
        assert!(!stream.is_null());
        unsafe {
            ((*(*stream).vtable).unknown.release)(stream.cast());
        }
        assert_eq!(
            unsafe { update_get_stream(&mut callback, 1, &mut stream) },
            E_FAIL
        );
        assert_eq!(
            unsafe { update_get_stream(&mut callback, 2, &mut stream) },
            E_FAIL
        );
    }

    #[test]
    fn streams_remain_item_indexed_with_directories_and_reordered_requests() {
        let mut first = tempfile::tempfile().unwrap();
        first.write_all(b"1").unwrap();
        first.seek(SeekFrom::Start(0)).unwrap();
        let mut second = tempfile::tempfile().unwrap();
        second.write_all(b"2").unwrap();
        second.seek(SeekFrom::Start(0)).unwrap();
        let items = vec![
            Item {
                name: "before".encode_utf16().collect(),
                directory: true,
                length: 0,
                source: None,
            },
            Item {
                name: "one".encode_utf16().collect(),
                directory: false,
                length: 1,
                source: Some(first.try_clone().unwrap()),
            },
            Item {
                name: "between".encode_utf16().collect(),
                directory: true,
                length: 0,
                source: None,
            },
            Item {
                name: "two".encode_utf16().collect(),
                directory: false,
                length: 1,
                source: Some(second.try_clone().unwrap()),
            },
            Item {
                name: "after".encode_utf16().collect(),
                directory: true,
                length: 0,
                source: None,
            },
        ];
        let streams = [None, Some(first), None, Some(second), None]
            .into_iter()
            .map(|stream| {
                stream.map(|file| {
                    Box::new(InputState {
                        file,
                        remaining: 1,
                        expected: 1,
                        failed: false,
                        errors: Arc::new(AtomicBool::new(false)),
                    })
                })
            })
            .collect();
        let mut state = Box::new(UpdateState {
            requested: vec![false; 5],
            items,
            streams,
            results: Vec::new(),
            cancelled: ptr::null(),
            failed: false,
            errors: Arc::new(AtomicBool::new(false)),
        });
        let mut callback = UpdateCallback {
            vtable: &UPDATE_VTABLE,
            state: &mut *state,
            refs: AtomicU32::new(1),
        };
        let mut stream = ptr::null_mut();
        for index in [2, 4, 0] {
            assert_eq!(
                unsafe { update_get_stream(&mut callback, index, &mut stream) },
                S_OK.0
            );
            assert!(stream.is_null());
        }
        for index in [3, 1] {
            assert_eq!(
                unsafe { update_get_stream(&mut callback, index, &mut stream) },
                S_OK.0
            );
            assert!(!stream.is_null());
            unsafe {
                ((*(*stream).vtable).unknown.release)(stream.cast());
            }
        }
        assert_eq!(state.requested, [false, true, false, true, false]);
    }

    fn metadata(name: &[u16], directory: bool, length: u64, has_source: bool) -> ItemMetadata<'_> {
        ItemMetadata {
            name,
            directory,
            length,
            has_source,
        }
    }

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().collect()
    }

    #[test]
    fn borrowed_metadata_accepts_files_and_directories_without_file_values() {
        let folder = wide("empty");
        let file = wide("日本語/@-entry.txt");
        assert!(
            validate_metadata([
                metadata(&folder, true, 0, false),
                metadata(&file, false, 7, true),
            ])
            .is_ok()
        );
    }

    #[test]
    fn borrowed_metadata_checks_source_association_and_zero_length_directories() {
        let file = wide("file");
        let dir = wide("dir");
        assert!(validate_metadata([metadata(&file, false, 0, false)]).is_err());
        assert!(validate_metadata([metadata(&dir, true, 0, true)]).is_err());
        assert!(validate_metadata([metadata(&dir, true, 1, false)]).is_err());
    }

    #[test]
    fn borrowed_metadata_rejects_invalid_unicode_and_paths() {
        let unpaired = [0xd800];
        let rooted = wide("/rooted");
        let traversal = wide("parent/../escape");
        assert!(validate_metadata([metadata(&unpaired, false, 1, true)]).is_err());
        assert!(validate_metadata([metadata(&rooted, false, 1, true)]).is_err());
        assert!(validate_metadata([metadata(&traversal, false, 1, true)]).is_err());
    }

    #[test]
    fn borrowed_metadata_rejects_ordinal_collisions_and_file_ancestors() {
        let upper = wide("Parent");
        let lower = wide("parent");
        let child = wide("parent/child");
        assert!(
            validate_metadata([
                metadata(&upper, false, 1, true),
                metadata(&lower, false, 1, true),
            ])
            .is_err()
        );
        assert!(
            validate_metadata([
                metadata(&upper, false, 1, true),
                metadata(&child, false, 1, true),
            ])
            .is_err()
        );
        assert!(
            validate_metadata([
                metadata(&child, false, 1, true),
                metadata(&upper, false, 1, true),
            ])
            .is_err()
        );
    }
}
