use std::{
    ffi::{OsStr, OsString, c_void},
    fs::File,
    os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Storage::FileSystem::{
            FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_TYPE_DISK, FILE_WRITE_DATA, GetFileType,
            SYNCHRONIZE,
        },
        System::{
            Com::CoCreateGuid,
            IO::{IO_STATUS_BLOCK, IO_STATUS_BLOCK_0},
            Memory::{
                CreateFileMappingW, FILE_MAP, FILE_MAP_READ, FILE_MAP_WRITE,
                MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, SECTION_MAP_READ,
                SECTION_MAP_WRITE, UnmapViewOfFile,
            },
            Threading::{INFINITE, WaitForSingleObject},
        },
    },
    core::PCWSTR,
};

use super::{
    Archive7zExtractionLease,
    seven_zip_decode_protocol::{self as protocol, FileFact, Receipt, Request},
    seven_zip_extract_plan::Validated7zExtractPlan,
};

const ENTRY_FLAG: &str = "--gpui-filer-7z-decode-helper";
pub(super) const MAX_DECODE_WORK: Duration = Duration::from_secs(60 * 60);
const FILE_MODE_INFORMATION: i32 = 16;
const STATUS_PENDING: i32 = 0x103;
const FILE_SYNCHRONOUS_IO_ALERT: u32 = 0x10;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NO_INTERMEDIATE_BUFFERING: u32 = 0x8;

struct DecodeTransport {
    _request_parent: OwnedHandle,
    request_child: OwnedHandle,
    source_child: OwnedHandle,
    spool_child: OwnedHandle,
    response: ResponseSection,
    args: Vec<OsString>,
}

pub(crate) struct SevenZipDecodeLaunch<'a> {
    lease: Archive7zExtractionLease,
    request: Request,
    transport: DecodeTransport,
    _source_borrow: &'a File,
    _spool_borrow: &'a mut File,
}

impl SevenZipDecodeLaunch<'_> {
    pub(super) fn args(&self) -> &[OsString] {
        &self.transport.args
    }

    pub(super) fn inherited_handles(&self) -> Vec<HANDLE> {
        vec![
            super::seven_zip_reader::raw_handle(&self.transport.request_child),
            super::seven_zip_reader::raw_handle(&self.transport.source_child),
            super::seven_zip_reader::raw_handle(&self.transport.spool_child),
            super::seven_zip_reader::raw_handle(&self.transport.response.child),
        ]
    }

    pub(super) fn lease(&self) -> &Archive7zExtractionLease {
        &self.lease
    }

    pub(super) fn image(&self) -> &super::application_image::ApplicationImageLease {
        self.lease.image()
    }

    pub(super) fn request(&self) -> &Request {
        &self.request
    }

    pub(super) fn read_receipt_after_drain(&self) -> Result<Receipt, ()> {
        let bytes = self.transport.response.copy_completed()?;
        protocol::decode_receipt(&bytes)
    }

    pub(super) fn held_source_and_spool_match(&self) -> bool {
        let source = HANDLE(self._source_borrow.as_raw_handle().cast());
        let spool = HANDLE((&*self._spool_borrow).as_raw_handle().cast());
        super::seven_zip_reader::source_facts(source)
            == Some((
                self.request.source.volume_serial,
                self.request.source.file_index,
                self.request.source.length,
            ))
            && super::seven_zip_reader::source_facts(spool)
                == Some((
                    self.request.spool.volume_serial,
                    self.request.spool.file_index,
                    self.request.plan.total_size(),
                ))
    }
}

/// Builds a contained transport around a held source and a CREATE_NEW spool
/// opened READ|WRITE|DELETE with FILE_SHARE_READ. Caller must keep all spool
/// aliases free of outstanding/concurrent I/O through drain and receipt validation.
pub(super) fn build_launch<'a>(
    lease: Archive7zExtractionLease,
    source: &'a File,
    spool: &'a mut File,
    plan: Arc<Validated7zExtractPlan>,
) -> Option<SevenZipDecodeLaunch<'a>> {
    // 呼び出し元は drain まで spool の alias 経由の I/O を行わない。
    let source_raw = HANDLE(source.as_raw_handle().cast());
    let spool_raw = HANDLE(spool.as_raw_handle().cast());
    let source_facts = super::seven_zip_reader::source_facts(source_raw)?;
    let spool_facts = super::seven_zip_reader::source_facts(spool_raw)?;
    if (spool_facts.2 != 0) || (source_facts.0 == spool_facts.0 && source_facts.1 == spool_facts.1)
    {
        return None;
    }
    let source_sync = super::seven_zip_reader::SyncSource::reopen(source_raw)?;
    if source_sync.facts != source_facts
        || !super::seven_zip_reader::is_local_synchronous_source(
            super::seven_zip_reader::raw_handle(&source_sync.handle),
        )
    {
        return None;
    }

    let guid = unsafe { CoCreateGuid() }.ok()?;
    let mut invocation_id = [0u8; 16];
    invocation_id[..4].copy_from_slice(&guid.data1.to_le_bytes());
    invocation_id[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    invocation_id[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    invocation_id[8..].copy_from_slice(&guid.data4);
    let request = Request {
        invocation_id,
        dll_path: lease
            .provider()
            .0
            .binding
            .zip_dll
            .path
            .to_string_lossy()
            .into_owned(),
        handles: [0; 3],
        source: FileFact {
            volume_serial: source_facts.0,
            file_index: source_facts.1,
            length: source_facts.2,
        },
        spool: FileFact {
            volume_serial: spool_facts.0,
            file_index: spool_facts.1,
            length: spool_facts.2,
        },
        plan,
    };
    // Handles are bound into the request after their exact-purpose duplicates exist.
    let response = ResponseSection::create()?;
    let source_child = super::seven_zip_reader::duplicate(
        super::seven_zip_reader::raw_handle(&source_sync.handle),
        FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
        true,
    )?;
    let spool_child = super::seven_zip_reader::duplicate(
        spool_raw,
        FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
        true,
    )?;

    let request = Request {
        handles: [
            super::seven_zip_reader::raw_handle(&source_child).0 as usize as u64,
            super::seven_zip_reader::raw_handle(&spool_child).0 as usize as u64,
            super::seven_zip_reader::raw_handle(&response.child).0 as usize as u64,
        ],
        ..request
    };
    let request_section = create_request_section(&request)?;
    if !protocol::valid_handle_roles(
        raw_u64(super::seven_zip_reader::raw_handle(&request_section.child)),
        &request,
    ) {
        return None;
    }
    let mapping_raw = raw_u64(super::seven_zip_reader::raw_handle(&request_section.child));
    Some(SevenZipDecodeLaunch {
        lease,
        request,
        transport: DecodeTransport {
            _request_parent: request_section.parent,
            request_child: request_section.child,
            source_child,
            spool_child,
            response,
            args: vec![ENTRY_FLAG.into(), mapping_raw.to_string().into()],
        },
        _source_borrow: source,
        _spool_borrow: spool,
    })
}

struct RequestSection {
    parent: OwnedHandle,
    child: OwnedHandle,
}

fn create_request_section(request: &Request) -> Option<RequestSection> {
    let frame = protocol::encode_request(request).ok()?;
    let section = create_section(frame.len())?;
    let view = map_view(
        super::seven_zip_reader::raw_handle(&section),
        FILE_MAP_WRITE,
        frame.len(),
    )?;
    unsafe { std::ptr::copy_nonoverlapping(frame.as_ptr(), view.0.Value.cast(), frame.len()) };
    drop(view);
    Some(RequestSection {
        parent: super::seven_zip_reader::duplicate(
            super::seven_zip_reader::raw_handle(&section),
            SECTION_MAP_READ.0,
            false,
        )?,
        child: super::seven_zip_reader::duplicate(
            super::seven_zip_reader::raw_handle(&section),
            SECTION_MAP_READ.0,
            true,
        )?,
    })
}

struct ResponseSection {
    parent: OwnedHandle,
    child: OwnedHandle,
}

impl ResponseSection {
    fn create() -> Option<Self> {
        let section = create_section(protocol::RECEIPT_LEN)?;
        Some(Self {
            parent: super::seven_zip_reader::duplicate(
                super::seven_zip_reader::raw_handle(&section),
                SECTION_MAP_READ.0,
                false,
            )?,
            child: super::seven_zip_reader::duplicate(
                super::seven_zip_reader::raw_handle(&section),
                SECTION_MAP_WRITE.0,
                true,
            )?,
        })
    }

    fn copy_completed(&self) -> Result<Vec<u8>, ()> {
        let view = map_view(
            super::seven_zip_reader::raw_handle(&self.parent),
            FILE_MAP_READ,
            protocol::RECEIPT_LEN,
        )
        .ok_or(())?;
        super::seven_zip_reader::read_own_process(view.0.Value.cast_const(), protocol::RECEIPT_LEN)
            .map_err(|_| ())
    }
}

fn create_section(length: usize) -> Option<OwnedHandle> {
    let raw = unsafe {
        CreateFileMappingW(
            HANDLE(-1isize as *mut c_void),
            None,
            PAGE_READWRITE,
            0,
            u32::try_from(length).ok()?,
            PCWSTR::null(),
        )
    }
    .ok()?;
    Some(unsafe { OwnedHandle::from_raw_handle(raw.0.cast()) })
}

struct MappedView(MEMORY_MAPPED_VIEW_ADDRESS);
impl Drop for MappedView {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.0);
        }
    }
}

fn map_view(handle: HANDLE, access: FILE_MAP, length: usize) -> Option<MappedView> {
    let view = unsafe { MapViewOfFile(handle, access, 0, 0, length) };
    (!view.Value.is_null()).then_some(MappedView(view))
}

pub(super) fn run_decode_from_process_args() -> Option<i32> {
    let mut args = std::env::args_os();
    let _exe = args.next()?;
    let flag = args.next()?;
    if flag != OsStr::new(ENTRY_FLAG) {
        return None;
    }
    let Some(raw) = args.next() else {
        return Some(1);
    };
    if args.next().is_some() {
        return Some(1);
    }
    let Some(text) = raw.to_str() else {
        return Some(1);
    };
    let Ok(raw) = text.parse::<u64>() else {
        return Some(1);
    };
    let Some(mapping) = super::seven_zip_reader::handle_from_raw(raw) else {
        return Some(1);
    };
    Some(unsafe { run_decode_child(mapping) })
}

unsafe fn run_decode_child(request_mapping: HANDLE) -> i32 {
    if !super::seven_zip_reader::valid_inheritable(request_mapping) {
        return 1;
    }
    let Some(frame) = (unsafe { read_request_mapping(request_mapping) }) else {
        return 1;
    };
    let Ok(request) = protocol::decode_request(&frame) else {
        return 1;
    };
    let mapping_raw = raw_u64(request_mapping);
    if !protocol::valid_handle_roles(mapping_raw, &request) {
        return 1;
    }
    let Some(source_raw) = super::seven_zip_reader::handle_from_raw(request.handles[0]) else {
        return 1;
    };
    let Some(spool_raw) = super::seven_zip_reader::handle_from_raw(request.handles[1]) else {
        return 1;
    };
    let Some(response_raw) = super::seven_zip_reader::handle_from_raw(request.handles[2]) else {
        return 1;
    };

    // Validate every inherited role before duplicating or owning any file handle.
    if !super::seven_zip_reader::valid_inheritable(source_raw)
        || !super::seven_zip_reader::valid_inheritable(spool_raw)
        || !super::seven_zip_reader::valid_inheritable(response_raw)
        || unsafe { GetFileType(source_raw) } != FILE_TYPE_DISK
        || unsafe { GetFileType(spool_raw) } != FILE_TYPE_DISK
        || super::seven_zip_reader::source_facts(source_raw).is_none()
        || super::seven_zip_reader::source_facts(spool_raw).is_none()
    {
        return 1;
    }
    let Some(response_view) = map_view(response_raw, FILE_MAP_WRITE, protocol::RECEIPT_LEN) else {
        return 1;
    };
    let source_initial = super::seven_zip_reader::source_facts(source_raw).unwrap();
    let spool_initial = super::seven_zip_reader::source_facts(spool_raw).unwrap();
    if source_initial
        != (
            request.source.volume_serial,
            request.source.file_index,
            request.source.length,
        )
        || spool_initial
            != (
                request.spool.volume_serial,
                request.spool.file_index,
                request.spool.length,
            )
        || (source_initial.0 == spool_initial.0 && source_initial.1 == spool_initial.1)
    {
        return 1;
    }

    let Some(source_owned) = super::seven_zip_reader::duplicate(
        source_raw,
        FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
        false,
    ) else {
        return 1;
    };
    let Some(spool_owned) = super::seven_zip_reader::duplicate(
        spool_raw,
        FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
        false,
    ) else {
        return 1;
    };
    let Some(_response_owned) =
        super::seven_zip_reader::duplicate(response_raw, SECTION_MAP_WRITE.0, false)
    else {
        return 1;
    };
    let Some(response_duplicate_view) = map_view(
        super::seven_zip_reader::raw_handle(&_response_owned),
        FILE_MAP_WRITE,
        protocol::RECEIPT_LEN,
    ) else {
        return 1;
    };
    drop(response_duplicate_view);
    let Some(source_check) = super::seven_zip_reader::duplicate(
        super::seven_zip_reader::raw_handle(&source_owned),
        FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
        false,
    ) else {
        return 1;
    };
    let Some(spool_check) = super::seven_zip_reader::duplicate(
        super::seven_zip_reader::raw_handle(&spool_owned),
        FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
        false,
    ) else {
        return 1;
    };
    if super::seven_zip_reader::source_facts(super::seven_zip_reader::raw_handle(&source_owned))
        != Some(source_initial)
        || super::seven_zip_reader::source_facts(super::seven_zip_reader::raw_handle(&spool_owned))
            != Some(spool_initial)
    {
        return 1;
    }
    unsafe {
        let _ = CloseHandle(source_raw);
        let _ = CloseHandle(spool_raw);
        let _ = CloseHandle(response_raw);
        let _ = CloseHandle(request_mapping);
    }
    let Some(spool_owned) = query_synchronous_spool_mode(spool_owned) else {
        return 1;
    };
    if !super::seven_zip_reader::is_local_synchronous_source(super::seven_zip_reader::raw_handle(
        &spool_owned,
    )) {
        return 1;
    }

    let source_sync = super::seven_zip_reader::SyncSource::reopen(
        super::seven_zip_reader::raw_handle(&source_owned),
    );
    let Some(source_sync) = source_sync else {
        return 1;
    };
    if source_sync.facts != source_initial
        || !super::seven_zip_reader::is_local_synchronous_source(
            super::seven_zip_reader::raw_handle(&source_sync.handle),
        )
    {
        return 1;
    }
    let source_file = unsafe { File::from_raw_handle(source_sync.handle.into_raw_handle()) };
    let spool_file = unsafe { File::from_raw_handle(spool_owned.into_raw_handle()) };
    let result = super::seven_zip_reader_sdk::extract_archive(
        Path::new(&request.dll_path),
        source_file,
        request.source.length,
        request.plan.clone(),
        spool_file,
        Arc::new(AtomicBool::new(false)),
    );
    if result.is_err() {
        return 1;
    }
    let Some(final_source) =
        super::seven_zip_reader::source_facts(super::seven_zip_reader::raw_handle(&source_check))
    else {
        return 1;
    };
    let Some(final_spool) =
        super::seven_zip_reader::source_facts(super::seven_zip_reader::raw_handle(&spool_check))
    else {
        return 1;
    };
    if final_source != source_initial
        || final_spool != (spool_initial.0, spool_initial.1, request.plan.total_size())
    {
        return 1;
    }
    let Ok(receipt) = protocol::make_receipt(&request) else {
        return 1;
    };
    let Ok(bytes) = protocol::encode_receipt(&receipt) else {
        return 1;
    };
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), response_view.0.Value.cast(), bytes.len())
    };
    0
}

struct ModeQuery {
    handle: OwnedHandle,
    iosb: IO_STATUS_BLOCK,
    mode: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationFile(
        file: HANDLE,
        iosb: *mut IO_STATUS_BLOCK,
        information: *mut c_void,
        length: u32,
        information_class: i32,
    ) -> i32;
}

fn query_synchronous_spool_mode(handle: OwnedHandle) -> Option<OwnedHandle> {
    let raw = super::seven_zip_reader::raw_handle(&handle);
    let mut state = Box::new(ModeQuery {
        handle,
        iosb: IO_STATUS_BLOCK {
            Anonymous: IO_STATUS_BLOCK_0 {
                Status: windows::Win32::Foundation::NTSTATUS(i32::MIN),
            },
            Information: usize::MAX,
        },
        mode: u32::MAX,
    });
    let status = unsafe {
        NtQueryInformationFile(
            raw,
            std::ptr::addr_of_mut!(state.iosb),
            std::ptr::addr_of_mut!(state.mode).cast(),
            std::mem::size_of::<u32>() as u32,
            FILE_MODE_INFORMATION,
        )
    };
    if status == STATUS_PENDING {
        let wait = unsafe { WaitForSingleObject(raw, INFINITE) };
        if wait != WAIT_OBJECT_0 {
            std::mem::forget(state);
            std::process::abort();
        }
    } else if status != 0 {
        return None;
    }
    let iosb_status = unsafe { state.iosb.Anonymous.Status.0 };
    if iosb_status == STATUS_PENDING {
        std::mem::forget(state);
        std::process::abort();
    }
    if iosb_status != 0
        || state.iosb.Information != std::mem::size_of::<u32>()
        || state.mode & FILE_NO_INTERMEDIATE_BUFFERING != 0
        || state.mode & (FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT) == 0
    {
        return None;
    }
    Some(state.handle)
}

unsafe fn read_request_mapping(handle: HANDLE) -> Option<Vec<u8>> {
    let header_view = map_view(handle, FILE_MAP_READ, 16)?;
    let header =
        super::seven_zip_reader::read_own_process(header_view.0.Value.cast_const(), 16).ok()?;
    drop(header_view);
    let length = protocol::bounded_request_frame_len(&header).ok()?;
    let view = map_view(handle, FILE_MAP_READ, length)?;
    let frame =
        super::seven_zip_reader::read_own_process(view.0.Value.cast_const(), length).ok()?;
    (frame.get(..header.len())? == header.as_slice()).then_some(frame)
}

fn raw_u64(handle: HANDLE) -> u64 {
    handle.0 as usize as u64
}

pub(super) fn run_parent<'a>(
    lease: Archive7zExtractionLease,
    source: &'a File,
    spool: &'a mut File,
    plan: Arc<Validated7zExtractPlan>,
    cwd: &Path,
    deadline: Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Receipt, super::windows_probe::operation_launcher::OperationOutcome> {
    let launch = build_launch(lease, source, spool, plan)
        .ok_or(super::windows_probe::operation_launcher::OperationOutcome::LaunchFailed)?;
    super::windows_probe::operation_launcher::run_seven_zip_decode(launch, cwd, deadline, cancelled)
}

#[cfg(test)]
#[path = "seven_zip_decode_tests.rs"]
mod tests;
