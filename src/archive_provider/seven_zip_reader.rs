#![cfg(windows)]
#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    ffi::c_void,
    fs::File,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    path::Path,
    ptr,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, DuplicateHandle, GENERIC_READ, GetHandleInformation, HANDLE,
            HANDLE_FLAG_INHERIT, SetHandleInformation,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, GetFileInformationByHandle,
            GetFileType, GetVolumeInformationByHandleW, ReOpenFile,
        },
        System::{
            Diagnostics::Debug::ReadProcessMemory,
            IO::{IO_STATUS_BLOCK, IO_STATUS_BLOCK_0},
            Memory::{
                CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
                MapViewOfFile, PAGE_READWRITE, SECTION_MAP_READ, SECTION_MAP_WRITE,
                UnmapViewOfFile,
            },
            Threading::GetCurrentProcess,
        },
    },
    core::PCWSTR,
};

use crate::archive_manifest::RawRecord;

const READ_FLAG: &str = "--gpui-filer-7z-read";
const INSPECT_FLAG: &str = "--gpui-filer-7z-inspect";
const MAX_WORK: Duration = Duration::from_secs(30);
const MAX_DLL_UNITS: usize = 32_767;
const READ_RESPONSE_MAGIC: &[u8; 8] = b"GP7ZRES\0";
const RESPONSE_HEADER: usize = 32;
const RESPONSE_CAPACITY: usize = super::seven_zip_reader_protocol::RESPONSE_CAPACITY;

struct ReaderTransport {
    _request_parent: OwnedHandle,
    request_child: OwnedHandle,
    source_child: OwnedHandle,
    response: ResponseSection,
    args: Vec<std::ffi::OsString>,
}

pub(crate) struct SevenZipReadLaunch {
    provider: super::Archive7zReadLease,
    image: super::application_image::ApplicationImageLease,
    transport: ReaderTransport,
}

pub(crate) struct SevenZipInspectionLaunch {
    lease: super::Archive7zExtractionLease,
    transport: ReaderTransport,
}

impl SevenZipReadLaunch {
    pub(super) fn args(&self) -> &[std::ffi::OsString] {
        &self.transport.args
    }
    pub(super) fn inherited_handles(&self) -> Vec<HANDLE> {
        vec![
            raw_handle(&self.transport.request_child),
            raw_handle(&self.transport.source_child),
            raw_handle(&self.transport.response.writer),
        ]
    }
    pub(super) fn provider(&self) -> &super::Archive7zReadLease {
        &self.provider
    }
    pub(super) fn image(&self) -> &super::application_image::ApplicationImageLease {
        &self.image
    }
    pub(super) fn read_records_after_drain(&self) -> Result<Vec<RawRecord>, ()> {
        let bytes = self
            .transport
            .response
            .copy_completed_frame(READ_RESPONSE_MAGIC)
            .map_err(|_| ())?;
        super::seven_zip_reader_protocol::decode_response(&bytes).map_err(|_| ())
    }
}

impl SevenZipInspectionLaunch {
    pub(super) fn args(&self) -> &[std::ffi::OsString] {
        &self.transport.args
    }
    pub(super) fn inherited_handles(&self) -> Vec<HANDLE> {
        vec![
            raw_handle(&self.transport.request_child),
            raw_handle(&self.transport.source_child),
            raw_handle(&self.transport.response.writer),
        ]
    }
    pub(super) fn lease(&self) -> &super::Archive7zExtractionLease {
        &self.lease
    }
    pub(super) fn indexed_records_after_drain(
        &self,
    ) -> Result<Vec<super::seven_zip_extract_plan::IndexedRecord>, ()> {
        let bytes = self
            .transport
            .response
            .copy_completed_frame(super::seven_zip_inspection_protocol::RESPONSE_MAGIC)
            .map_err(|_| ())?;
        super::seven_zip_inspection_protocol::decode_response(&bytes)
    }
}

struct ResponseSection {
    readable: OwnedHandle,
    writer: OwnedHandle,
}

pub(crate) fn build_read_launch(
    provider: super::Archive7zReadLease,
    image: super::application_image::ApplicationImageLease,
    source: &File,
) -> Option<SevenZipReadLaunch> {
    let transport = build_transport(
        &provider,
        source,
        READ_FLAG,
        super::seven_zip_reader_protocol::REQUEST_MAGIC,
    )?;
    Some(SevenZipReadLaunch {
        provider,
        image,
        transport,
    })
}

pub(crate) fn build_inspection_launch(
    lease: super::Archive7zExtractionLease,
    source: &File,
) -> Option<SevenZipInspectionLaunch> {
    let transport = build_transport(
        lease.provider(),
        source,
        INSPECT_FLAG,
        super::seven_zip_inspection_protocol::REQUEST_MAGIC,
    )?;
    Some(SevenZipInspectionLaunch { lease, transport })
}

fn build_transport(
    provider: &super::Archive7zReadLease,
    source: &File,
    flag: &str,
    request_magic: &[u8; 8],
) -> Option<ReaderTransport> {
    let original = HANDLE(source.as_raw_handle().cast());
    let original_facts = source_facts(original)?;
    let reopened = SyncSource::reopen(original)?;
    if reopened.facts != original_facts {
        return None;
    }
    if !is_local_synchronous_source(raw_handle(&reopened.handle)) {
        return None;
    }
    let (volume_serial, file_index, source_length) = original_facts;
    let source_child = duplicate(raw_handle(&reopened.handle), GENERIC_READ.0, true)?;

    let response = create_response_section()?;
    let request = super::seven_zip_reader_protocol::Request {
        dll_path: provider.0.binding.zip_dll.path.to_str()?.to_owned(),
        source_raw: raw_handle(&source_child).0 as usize as u64,
        response_raw: raw_handle(&response.writer).0 as usize as u64,
        volume_serial,
        file_index,
        source_length,
    };
    let frame =
        super::seven_zip_reader_protocol::encode_request_for_magic(&request, request_magic).ok()?;
    let (request_parent, request_child) = create_request_section(&frame)?;
    let arg = raw_handle(&request_child).0 as usize;
    if !super::seven_zip_reader_protocol::valid_handle_roles(
        arg as u64,
        request.source_raw,
        request.response_raw,
    ) {
        return None;
    }
    Some(ReaderTransport {
        _request_parent: request_parent,
        request_child,
        source_child,
        response,
        args: vec![flag.into(), arg.to_string().into()],
    })
}

fn create_response_section() -> Option<ResponseSection> {
    let section = unsafe {
        CreateFileMappingW(
            HANDLE(-1isize as *mut c_void),
            None,
            PAGE_READWRITE,
            0,
            RESPONSE_CAPACITY as u32,
            PCWSTR::null(),
        )
    }
    .ok()?;
    let section = unsafe { OwnedHandle::from_raw_handle(section.0.cast()) };
    // 親の参照権限を読み取りだけに絞り、継承可能な書き込み handle は子の役割に限定する。
    let readable = duplicate(raw_handle(&section), SECTION_MAP_READ.0, false)?;
    let writer = duplicate(raw_handle(&section), SECTION_MAP_WRITE.0, true)?;
    Some(ResponseSection { readable, writer })
}

fn create_request_section(frame: &[u8]) -> Option<(OwnedHandle, OwnedHandle)> {
    if frame.is_empty() || frame.len() > REQUEST_MAX_BYTES {
        return None;
    }
    let section = unsafe {
        CreateFileMappingW(
            HANDLE(-1isize as *mut c_void),
            None,
            PAGE_READWRITE,
            0,
            frame.len() as u32,
            PCWSTR::null(),
        )
    }
    .ok()?;
    let section = unsafe { OwnedHandle::from_raw_handle(section.0.cast()) };
    let view = unsafe { MapViewOfFile(raw_handle(&section), FILE_MAP_WRITE, 0, 0, frame.len()) };
    if view.Value.is_null() {
        return None;
    }
    unsafe { ptr::copy_nonoverlapping(frame.as_ptr(), view.Value.cast(), frame.len()) };
    let _ = unsafe { UnmapViewOfFile(view) };
    let parent = duplicate(raw_handle(&section), SECTION_MAP_READ.0, false)?;
    let child = duplicate(raw_handle(&section), SECTION_MAP_READ.0, true)?;
    Some((parent, child))
}

const REQUEST_MAX_BYTES: usize =
    super::seven_zip_reader_protocol::REQUEST_HEADER + MAX_DLL_UNITS * 2;

impl ResponseSection {
    fn copy_completed_frame(&self, expected_magic: &[u8; 8]) -> io::Result<Vec<u8>> {
        let view = unsafe {
            MapViewOfFile(
                raw_handle(&self.readable),
                FILE_MAP_READ,
                0,
                0,
                RESPONSE_CAPACITY,
            )
        };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mapped = MappedView(view);
        let header = read_own_process(view.Value.cast_const(), RESPONSE_HEADER)?;
        if &header[..8] != expected_magic {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response header was not finalized",
            ));
        }
        let payload_len = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
        let frame_len = RESPONSE_HEADER
            .checked_add(payload_len)
            .filter(|n| *n <= RESPONSE_CAPACITY)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "response exceeds capacity")
            })?;
        // Job drain 後に OS のコピー API を使い、外部書き込み view への Rust 参照を作らない。
        let frame = read_own_process(view.Value.cast_const(), frame_len)?;
        if frame.get(..RESPONSE_HEADER) != Some(header.as_slice()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response header changed during detached copy",
            ));
        }
        drop(mapped);
        Ok(frame)
    }
}

struct MappedView(MEMORY_MAPPED_VIEW_ADDRESS);
impl Drop for MappedView {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.0);
        }
    }
}

pub(super) fn read_own_process(source: *const c_void, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; length];
    let mut read = 0usize;
    unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            source,
            bytes.as_mut_ptr().cast(),
            length,
            Some(&mut read),
        )
    }
    .map_err(|_| io::Error::last_os_error())?;
    if read != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short mapped read",
        ));
    }
    Ok(bytes)
}

pub(super) fn duplicate(source: HANDLE, access: u32, inheritable: bool) -> Option<OwnedHandle> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = HANDLE::default();
    unsafe {
        DuplicateHandle(
            process,
            source,
            process,
            &mut duplicate,
            access,
            inheritable,
            Default::default(),
        )
    }
    .ok()?;
    if duplicate.is_invalid() || duplicate.0.is_null() {
        unsafe {
            let _ = CloseHandle(duplicate);
        }
        return None;
    }
    if inheritable
        && unsafe { SetHandleInformation(duplicate, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .is_err()
    {
        unsafe {
            let _ = CloseHandle(duplicate);
        }
        return None;
    }
    Some(unsafe { OwnedHandle::from_raw_handle(duplicate.0.cast()) })
}

pub(super) fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle().cast())
}

pub(super) fn source_facts(handle: HANDLE) -> Option<(u32, u64, u64)> {
    if handle.is_invalid() || handle.0.is_null() || unsafe { GetFileType(handle) } != FILE_TYPE_DISK
    {
        return None;
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(handle, &mut info).ok()?;
    }
    let flags = info.dwFileAttributes;
    if flags
        & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DEVICE.0)
        != 0
    {
        return None;
    }
    let length = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    let mut filesystem = [0u16; 64];
    unsafe {
        GetVolumeInformationByHandleW(handle, None, None, None, None, Some(&mut filesystem))
            .ok()?;
    }
    let filesystem = String::from_utf16_lossy(&filesystem)
        .trim_end_matches('\0')
        .to_ascii_uppercase();
    if filesystem != "NTFS" && filesystem != "EXFAT" {
        return None;
    }
    Some((info.dwVolumeSerialNumber, index, length))
}

pub(super) struct SyncSource {
    pub(super) handle: OwnedHandle,
    pub(super) facts: (u32, u64, u64),
}

impl SyncSource {
    pub(super) fn reopen(original: HANDLE) -> Option<Self> {
        // OVERLAPPED付きの入力を同期ハンドルで開き直し、IOSBを呼び出し中だけ有効にする。
        let handle = unsafe {
            ReOpenFile(
                original,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAGS_AND_ATTRIBUTES(0),
            )
        }
        .ok()?;
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0.cast()) };
        let raw = raw_handle(&handle);
        let facts = source_facts(raw)?;
        Some(Self { handle, facts })
    }
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

pub(super) fn is_local_synchronous_source(handle: HANDLE) -> bool {
    const FILE_IS_REMOTE_DEVICE_INFORMATION: i32 = 51;
    let mut iosb = IO_STATUS_BLOCK {
        Anonymous: IO_STATUS_BLOCK_0 {
            Status: windows::Win32::Foundation::NTSTATUS(i32::MIN),
        },
        Information: usize::MAX,
    };
    let mut is_remote = u8::MAX;
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut iosb,
            (&mut is_remote as *mut u8).cast(),
            std::mem::size_of::<u8>() as u32,
            FILE_IS_REMOTE_DEVICE_INFORMATION,
        )
    };
    remote_query_succeeded(
        status,
        unsafe { iosb.Anonymous.Status.0 },
        iosb.Information,
        is_remote,
    )
}

fn remote_query_succeeded(
    status: i32,
    iosb_status: i32,
    information: usize,
    is_remote: u8,
) -> bool {
    status == 0 && iosb_status == 0 && information == 1 && is_remote == 0
}

pub(super) fn run_reader_from_process_args() -> Option<i32> {
    run_from_process_args(READ_FLAG, false)
}

pub(super) fn run_inspection_from_process_args() -> Option<i32> {
    run_from_process_args(INSPECT_FLAG, true)
}

fn run_from_process_args(expected_flag: &str, inspection: bool) -> Option<i32> {
    let mut args = std::env::args_os();
    let _program = args.next()?;
    let Some(flag) = args.next() else {
        return None;
    };
    if flag != expected_flag {
        return None;
    }
    let Some(raw) = args.next() else {
        return Some(1);
    };
    if args.next().is_some() {
        return Some(1);
    }
    let Ok(raw) = raw.to_string_lossy().parse::<usize>() else {
        return Some(1);
    };
    Some(unsafe { run_reader_handle(HANDLE(raw as *mut c_void), inspection) })
}

unsafe fn run_reader_handle(raw_request: HANDLE, inspection: bool) -> i32 {
    let Some(request_bytes) = (unsafe { read_request_mapping(raw_request) }) else {
        return 1;
    };
    let request = if inspection {
        super::seven_zip_reader_protocol::decode_request_for_magic(
            &request_bytes,
            super::seven_zip_inspection_protocol::REQUEST_MAGIC,
        )
    } else {
        super::seven_zip_reader_protocol::decode_request(&request_bytes)
    };
    let Ok(request) = request else {
        return 1;
    };
    let request_raw = raw_request.0 as usize as u64;
    if !super::seven_zip_reader_protocol::valid_handle_roles(
        request_raw,
        request.source_raw,
        request.response_raw,
    ) {
        return 1;
    }
    let source = handle_from_raw(request.source_raw);
    let response = handle_from_raw(request.response_raw);
    let (Some(source), Some(response)) = (source, response) else {
        return 1;
    };
    if !valid_inheritable(source)
        || !valid_inheritable(response)
        || unsafe { GetFileType(source) } != FILE_TYPE_DISK
    {
        return 1;
    }
    let Some((volume, index, length)) = source_facts(source) else {
        return 1;
    };
    if (volume, index, length)
        != (
            request.volume_serial,
            request.file_index,
            request.source_length,
        )
    {
        return 1;
    }
    let Some(response_view) = response_writer_view(response) else {
        return 1;
    };
    let Some(source_sync) = SyncSource::reopen(source) else {
        return 1;
    };
    if source_sync.facts != (volume, index, length) {
        return 1;
    }
    if !is_local_synchronous_source(raw_handle(&source_sync.handle)) {
        return 1;
    }
    let source_file = unsafe { File::from_raw_handle(source_sync.handle.into_raw_handle()) };
    let records = if inspection {
        let records = super::seven_zip_reader_sdk::inspect_archive(
            Path::new(&request.dll_path),
            source_file,
            request.source_length,
        );
        let written = unsafe {
            super::seven_zip_inspection_protocol::write_response(
                response_view.0.Value.cast(),
                RESPONSE_CAPACITY,
                records.as_deref().map_err(|_| ()),
            )
        };
        return if written.is_err() {
            1
        } else if records.is_err() {
            2
        } else {
            0
        };
    } else {
        super::seven_zip_reader_sdk::read_archive(
            Path::new(&request.dll_path),
            source_file,
            request.source_length,
        )
    };
    let result = unsafe {
        super::seven_zip_reader_protocol::write_response(
            response_view.0.Value.cast(),
            RESPONSE_CAPACITY,
            records.as_deref().map_err(|_| ()),
        )
    };
    if result.is_err() {
        1
    } else if records.is_err() {
        2
    } else {
        0
    }
}

unsafe fn read_request_mapping(handle: HANDLE) -> Option<Vec<u8>> {
    if !valid_inheritable(handle) {
        return None;
    }
    let header_view = unsafe {
        MapViewOfFile(
            handle,
            FILE_MAP_READ,
            0,
            0,
            super::seven_zip_reader_protocol::REQUEST_HEADER,
        )
    };
    if header_view.Value.is_null() {
        return None;
    }
    let header = read_own_process(
        header_view.Value.cast_const(),
        super::seven_zip_reader_protocol::REQUEST_HEADER,
    )
    .ok()?;
    let _ = unsafe { UnmapViewOfFile(header_view) };
    let length = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
    if !(super::seven_zip_reader_protocol::REQUEST_HEADER..=REQUEST_MAX_BYTES).contains(&length) {
        return None;
    }
    let full_view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, length) };
    if full_view.Value.is_null() {
        return None;
    }
    let bytes = read_own_process(full_view.Value.cast_const(), length).ok()?;
    let _ = unsafe { UnmapViewOfFile(full_view) };
    if bytes.get(..super::seven_zip_reader_protocol::REQUEST_HEADER)? != header.as_slice() {
        return None;
    }
    Some(bytes)
}

fn response_writer_view(handle: HANDLE) -> Option<MappedView> {
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, RESPONSE_CAPACITY) };
    (!view.Value.is_null()).then_some(MappedView(view))
}

pub(super) fn valid_inheritable(handle: HANDLE) -> bool {
    if handle.is_invalid() || handle.0.is_null() {
        return false;
    }
    let raw = handle.0 as usize as u64;
    if raw > usize::MAX as u64 || (usize::MAX as u64 - 5..=usize::MAX as u64).contains(&raw) {
        return false;
    }
    let mut flags = 0u32;
    unsafe {
        GetHandleInformation(handle, &mut flags).is_ok() && flags & HANDLE_FLAG_INHERIT.0 != 0
    }
}

pub(super) fn handle_from_raw(raw: u64) -> Option<HANDLE> {
    if raw == 0
        || raw > usize::MAX as u64
        || (usize::MAX as u64 - 5..=usize::MAX as u64).contains(&raw)
    {
        return None;
    }
    Some(HANDLE(raw as usize as *mut c_void))
}

// 呼び出し元が長い期限を渡しても、読み取り処理の上限を延ばさない。
pub(super) fn bounded_deadline(caller: Instant) -> Instant {
    bounded_deadline_from(caller, Instant::now())
}

fn bounded_deadline_from(caller: Instant, started: Instant) -> Instant {
    caller.min(started + MAX_WORK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reader_deadline_never_exceeds_thirty_seconds_and_can_be_shortened() {
        let now = Instant::now();
        assert_eq!(
            bounded_deadline_from(now + Duration::from_secs(60), now),
            now + Duration::from_secs(30)
        );
        let short = now + Duration::from_secs(2);
        assert_eq!(bounded_deadline_from(short, now), short);
    }

    #[test]
    fn remote_device_query_requires_synchronous_complete_exact_local_result() {
        assert!(remote_query_succeeded(0, 0, 1, 0));
        for invalid in [
            (0xC000_0001u32 as i32, 0, 1, 0),
            (0x0000_0103, 0, 1, 0),
            (0, 0xC000_0001u32 as i32, 1, 0),
            (0, 0, 0, 0),
            (0, 0, usize::MAX, 0),
            (0, 0, 1, 1),
            (0, 0, 1, u8::MAX),
        ] {
            assert!(!remote_query_succeeded(
                invalid.0, invalid.1, invalid.2, invalid.3
            ));
        }
    }

    #[test]
    fn overlapped_parent_input_is_reopened_before_native_locality_query() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let fixture = tempfile::NamedTempFile::new().unwrap();
        let input = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(fixture.path())
            .unwrap();
        let original = HANDLE(input.as_raw_handle().cast());
        let expected = source_facts(original).expect("local fixture has supported basic facts");
        let reopened = SyncSource::reopen(original)
            .expect("the overlapped input can be reopened synchronously");
        assert_eq!(reopened.facts, expected);
        assert!(is_local_synchronous_source(raw_handle(&reopened.handle)));
    }

    #[test]
    #[ignore = "requires GPUI_FILER_V6_TEST_EXECUTABLE and GPUI_FILER_7ZIP_TEST_EXECUTABLE"]
    fn installed_reader_lists_supported_archives_and_rejects_incomplete_or_encrypted_inputs() {
        use crate::archive_provider::{ArchiveProviderOwner, discover_from_path};

        let app = std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
            .map(PathBuf::from)
            .expect("set GPUI_FILER_V6_TEST_EXECUTABLE");
        let provider_path = std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .map(PathBuf::from)
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE");
        let provider_dir = provider_path.parent().expect("provider directory");
        let provider = discover_from_path(
            Some(provider_dir.as_os_str().to_owned()),
            std::env::current_dir().unwrap(),
        )
        .expect("installed 7z provider");
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(revision, Some(provider)));
        let capability = owner
            .capture_7z_read_capability()
            .expect("single 7z read capability");
        let fixtures = std::env::var_os("GPUI_FILER_7ZIP_FIXTURE_ROOT")
            .map(PathBuf::from)
            .expect("set GPUI_FILER_7ZIP_FIXTURE_ROOT to the prepared NTFS or exFAT fixture root");
        let expected = [
            ("root-a", true, None),
            ("root-a/empty-dir", true, None),
            ("root-a/empty-file", false, Some(0)),
            ("root-a/nested", true, None),
            ("root-a/nested/a.txt", false, Some(6000u64)),
            ("root-b", true, None),
            ("root-b/-name.bin", false, Some(2048u64)),
            ("日本語.txt", false, Some(20u64)),
        ];
        for archive_name in ["default-lzma2.7z", "lzma.7z", "deflate.7z"] {
            let archive = fixtures.join(archive_name);
            let file = File::open(&archive)
                .unwrap_or_else(|error| panic!("cannot open {archive_name}: {error}"));
            let before = std::fs::metadata(&archive).unwrap();
            let manifest = owner
                .read_7z_archive(
                    &capability,
                    super::super::application_image::lease_explicit_path_for_test(&app)
                        .expect("test executable lease"),
                    &file,
                    &std::env::current_dir().unwrap(),
                    Instant::now() + Duration::from_secs(90),
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .unwrap_or_else(|error| {
                    panic!("{archive_name} should list successfully: {error:?}")
                });
            let actual = manifest
                .nodes
                .iter()
                .map(|node| (node.components.join("/"), node.kind, node.declared_size))
                .collect::<Vec<_>>();
            assert_eq!(actual.len(), expected.len(), "{archive_name}: entry count");
            for ((path, kind, size), (expected_path, directory, expected_size)) in
                actual.iter().zip(expected)
            {
                assert_eq!(path, expected_path, "{archive_name}: path");
                assert_eq!(
                    *kind,
                    if directory {
                        crate::archive_manifest::NodeKind::Directory
                    } else {
                        crate::archive_manifest::NodeKind::File
                    },
                    "{archive_name}: kind for {path}"
                );
                assert_eq!(*size, expected_size, "{archive_name}: size for {path}");
            }
            assert_eq!(std::fs::metadata(&archive).unwrap().len(), before.len());
        }
        for archive_name in ["empty.7z"] {
            let archive = fixtures.join(archive_name);
            let file = File::open(&archive)
                .unwrap_or_else(|error| panic!("cannot open {archive_name}: {error}"));
            let empty = owner
                .read_7z_archive(
                    &capability,
                    super::super::application_image::lease_explicit_path_for_test(&app).unwrap(),
                    &file,
                    &std::env::current_dir().unwrap(),
                    Instant::now() + Duration::from_secs(30),
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .unwrap_or_else(|error| panic!("{archive_name} should list: {error:?}"));
            assert!(
                empty.nodes.is_empty(),
                "{archive_name} should have no entries"
            );
        }
        let archive_name = "unix-regular.7z";
        let archive = fixtures.join(archive_name);
        let regular = File::open(&archive)
            .unwrap_or_else(|error| panic!("cannot open {archive_name}: {error}"));
        let regular_manifest = owner
            .read_7z_archive(
                &capability,
                super::super::application_image::lease_explicit_path_for_test(&app).unwrap(),
                &regular,
                &std::env::current_dir().unwrap(),
                Instant::now() + Duration::from_secs(30),
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap_or_else(|error| panic!("{archive_name} should list: {error:?}"));
        assert_eq!(
            regular_manifest.nodes.len(),
            1,
            "{archive_name} entry count"
        );
        for archive_name in [
            "unix-symlink.7z",
            "unix-fifo.7z",
            "unix-character-device.7z",
            "unix-block-device.7z",
            "unix-socket.7z",
            "unix-directory-kind-mismatch.7z",
        ] {
            let file = File::open(fixtures.join(archive_name)).unwrap();
            assert!(
                owner
                    .read_7z_archive(
                        &capability,
                        super::super::application_image::lease_explicit_path_for_test(&app)
                            .unwrap(),
                        &file,
                        &std::env::current_dir().unwrap(),
                        Instant::now() + Duration::from_secs(30),
                        &std::sync::atomic::AtomicBool::new(false),
                    )
                    .is_err(),
                "{archive_name} must be rejected"
            );
        }
        for archive_name in [
            "encrypted-data.7z",
            "encrypted-header.7z",
            "prefixed.7z",
            "trailing.7z",
            "corrupt.7z",
            "truncated.7z",
            "reserved-name.7z",
            "traversal-name.7z",
            "implicit-parent-alias.7z",
        ] {
            let archive = fixtures.join(archive_name);
            let file = File::open(&archive).unwrap();
            assert!(
                owner
                    .read_7z_archive(
                        &capability,
                        super::super::application_image::lease_explicit_path_for_test(&app)
                            .unwrap(),
                        &file,
                        &std::env::current_dir().unwrap(),
                        Instant::now() + Duration::from_secs(30),
                        &std::sync::atomic::AtomicBool::new(false)
                    )
                    .is_err(),
                "{archive_name} must not list successfully"
            );
        }
    }

    #[test]
    #[ignore = "requires installed app/provider and prepared NTFS or exFAT SDK fixture root"]
    fn installed_sdk_inspection_returns_original_indices_crc_and_ranges() {
        use crate::archive_manifest::NodeKind;
        use crate::archive_provider::{ArchiveProviderOwner, discover_from_path};

        let app = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("set GPUI_FILER_V6_TEST_EXECUTABLE"),
        );
        let provider_path = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE"),
        );
        let fixtures = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_FIXTURE_ROOT")
                .expect("set GPUI_FILER_7ZIP_FIXTURE_ROOT to prepared NTFS or exFAT fixtures"),
        );
        let provider = discover_from_path(
            Some(provider_path.parent().unwrap().as_os_str().to_owned()),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let owner = ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(revision, Some(provider)));
        let capability = owner.capture_7z_read_capability().unwrap();
        let lease = owner
            .lease_7z_extraction(
                &capability,
                super::super::application_image::lease_explicit_path_for_test(&app).unwrap(),
            )
            .expect("exact current read admission");
        let expected = [
            (0, "root-a", NodeKind::Directory, None, None),
            (1, "root-a/empty-dir", NodeKind::Directory, None, None),
            (2, "root-a/nested", NodeKind::Directory, None, None),
            (3, "root-b", NodeKind::Directory, None, None),
            (4, "root-a/empty-file", NodeKind::File, Some(0), None),
            (
                5,
                "root-a/nested/a.txt",
                NodeKind::File,
                Some(6000),
                Some(0x61df3919),
            ),
            (
                6,
                "root-b/-name.bin",
                NodeKind::File,
                Some(2048),
                Some(0x9f5edd58),
            ),
            (7, "日本語.txt", NodeKind::File, Some(20), Some(0x003e1c57)),
        ];
        for archive_name in ["default-lzma2.7z", "lzma.7z", "deflate.7z"] {
            let archive = fixtures.join(archive_name);
            let before = std::fs::read(&archive).unwrap();
            let file = File::open(&archive).unwrap();
            let outer_strong = std::sync::Arc::strong_count(&lease.0);
            let launch = build_inspection_launch(lease.clone(), &file)
                .expect("inspection launch keeps the exact outer route lease");
            assert_eq!(std::sync::Arc::strong_count(&lease.0), outer_strong + 1);
            drop(launch);
            assert_eq!(std::sync::Arc::strong_count(&lease.0), outer_strong);
            let plan = owner
                .inspect_7z_archive(
                    &lease,
                    &file,
                    &std::env::current_dir().unwrap(),
                    Instant::now() + Duration::from_secs(90),
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .unwrap_or_else(|error| panic!("{archive_name} inspection failed: {error:?}"));
            assert_eq!(
                std::sync::Arc::strong_count(&lease.0),
                1,
                "launch clone released after complete run"
            );
            assert_eq!(
                plan.entries().len(),
                expected.len(),
                "{archive_name} indexed records"
            );
            let expected_by_index = expected
                .iter()
                .map(|(index, path, kind, size, crc)| (*index, (*path, *kind, *size, *crc)))
                .collect::<std::collections::BTreeMap<_, _>>();
            let mut expected_offset = 0u64;
            let mut expected_ranges = std::collections::BTreeMap::new();
            for (index, (_, kind, size, _)) in &expected_by_index {
                if *kind == NodeKind::File {
                    let start = expected_offset;
                    expected_offset += size.expect("files have declared sizes");
                    expected_ranges.insert(*index, (start, expected_offset));
                }
            }
            for entry in plan.entries() {
                let (path, kind, size, crc) = expected_by_index[&entry.index()];
                assert_eq!(entry.components().join("/"), path, "{archive_name} index");
                assert_eq!(entry.kind(), kind, "{archive_name} kind {path}");
                assert_eq!(entry.declared_size(), size, "{archive_name} size {path}");
                assert_eq!(entry.crc32(), crc, "{archive_name} CRC {path}");
                if kind == NodeKind::Directory {
                    assert!(
                        entry.spool_range().is_none(),
                        "directory has no spool range: {path}"
                    );
                } else {
                    let range = entry.spool_range().expect("file has a spool range");
                    assert_eq!(
                        (range.start, range.end),
                        expected_ranges[&entry.index()],
                        "{archive_name} file range for {path}"
                    );
                }
            }
            let expected_nodes = expected
                .iter()
                .map(|(_, path, kind, size, _)| ((*path).to_owned(), (*kind, *size)))
                .collect::<std::collections::BTreeMap<_, _>>();
            let actual_nodes = plan
                .nodes()
                .iter()
                .map(|node| {
                    (
                        node.components().join("/"),
                        (node.kind(), node.declared_size()),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                actual_nodes, expected_nodes,
                "{archive_name} canonical nodes by path"
            );
            assert_eq!(expected_offset, 8068, "{archive_name} file-byte total");
            assert_eq!(plan.total_size(), 8068, "{archive_name} planned total");
            assert_eq!(
                std::fs::read(&archive).unwrap(),
                before,
                "inspection changed source"
            );
        }
        for archive_name in ["single-empty-file.7z", "unix-regular.7z"] {
            let archive = fixtures.join(archive_name);
            let before = std::fs::read(&archive).unwrap();
            let file = File::open(&archive).unwrap();
            let plan = owner
                .inspect_7z_archive(
                    &lease,
                    &file,
                    &std::env::current_dir().unwrap(),
                    Instant::now() + Duration::from_secs(30),
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .unwrap_or_else(|error| panic!("{archive_name} inspection failed: {error:?}"));
            assert_eq!(plan.total_size(), 0);
            assert_eq!(plan.entries().len(), 1);
            let entry = &plan.entries()[0];
            assert_eq!(entry.index(), 0);
            assert_eq!(entry.components().join("/"), "empty-file");
            assert_eq!(entry.kind(), NodeKind::File);
            assert_eq!(entry.declared_size(), Some(0));
            assert_eq!(
                entry.spool_range().map(|range| (range.start, range.end)),
                Some((0, 0))
            );
            assert_eq!(std::sync::Arc::strong_count(&lease.0), 1);
            assert_eq!(
                std::fs::read(&archive).unwrap(),
                before,
                "inspection changed source"
            );
        }
        for archive_name in [
            "empty.7z",
            "encrypted-data.7z",
            "encrypted-header.7z",
            "unix-symlink.7z",
            "unix-fifo.7z",
            "unix-character-device.7z",
            "unix-block-device.7z",
            "unix-socket.7z",
            "unix-directory-kind-mismatch.7z",
            "prefixed.7z",
            "trailing.7z",
            "corrupt.7z",
            "truncated.7z",
            "reserved-name.7z",
            "traversal-name.7z",
            "implicit-parent-alias.7z",
        ] {
            let archive = fixtures.join(archive_name);
            let file =
                File::open(&archive).unwrap_or_else(|error| panic!("{archive_name}: {error}"));
            let result = owner.inspect_7z_archive(
                &lease,
                &file,
                &std::env::current_dir().unwrap(),
                Instant::now() + Duration::from_secs(30),
                &std::sync::atomic::AtomicBool::new(false),
            );
            if archive_name == "empty.7z" {
                assert!(result.unwrap().nodes().is_empty());
            } else {
                assert!(result.is_err(), "{archive_name} must be rejected");
            }
        }
    }

    #[test]
    fn child_roles_reject_disk_files_where_sections_are_required() {
        use std::os::windows::io::AsRawHandle;
        let file = tempfile::tempfile().unwrap();
        let raw = HANDLE(file.as_raw_handle().cast());
        let inheritable = duplicate(raw, GENERIC_READ.0, true).unwrap();
        let handle = raw_handle(&inheritable);
        assert!(valid_inheritable(handle));
        assert!(unsafe { read_request_mapping(handle) }.is_none());
        assert!(response_writer_view(handle).is_none());

        let section = create_response_section().unwrap();
        let section_as_source =
            duplicate(raw_handle(&section.readable), GENERIC_READ.0, true).unwrap();
        assert!(valid_inheritable(raw_handle(&section_as_source)));
        assert!(source_facts(raw_handle(&section_as_source)).is_none());
    }

    #[test]
    fn response_role_rejects_a_section_smaller_than_the_fixed_frame() {
        let section = unsafe {
            CreateFileMappingW(
                HANDLE(-1isize as *mut c_void),
                None,
                PAGE_READWRITE,
                0,
                4096,
                PCWSTR::null(),
            )
        }
        .unwrap();
        let section = unsafe { OwnedHandle::from_raw_handle(section.0.cast()) };
        let child = duplicate(raw_handle(&section), SECTION_MAP_WRITE.0, true).unwrap();
        assert!(valid_inheritable(raw_handle(&child)));
        assert!(response_writer_view(raw_handle(&child)).is_none());
    }
}
