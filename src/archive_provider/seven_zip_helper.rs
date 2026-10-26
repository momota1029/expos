//! Same-executable 7-Zip SDK helper entry and child-side capability admission.

use std::{
    ffi::{OsStr, OsString, c_void},
    fs::File,
    io::{Seek, SeekFrom},
    os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    path::Path,
    sync::atomic::AtomicBool,
};
use windows::Win32::{
    Foundation::{
        CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE,
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType,
        GetVolumeInformationByHandleW, SYNCHRONIZE,
    },
    System::{
        Memory::{
            FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, SECTION_MAP_READ,
            UnmapViewOfFile,
        },
        Threading::GetCurrentProcess,
    },
};

const ENTRY_FLAG: &str = "--gpui-filer-7z-helper";

/// Ordinary invocation returns `None`; recognized helper forms always return a result.
pub(super) fn run_helper_from_process_args() -> Option<i32> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let Some(result) = classify_helper_args(&args) else {
        return None;
    };
    let Ok(raw) = result else {
        return Some(1);
    };
    // SAFETY: main の最初の処理として GPUI 前にのみ呼び、元 handle を並行して再利用する処理はない。
    Some(unsafe { run_helper_handle(HANDLE(raw as usize as *mut c_void)) })
}

fn classify_helper_args(args: &[OsString]) -> Option<Result<u64, ()>> {
    let first = args.first()?;
    if first != OsStr::new(ENTRY_FLAG) {
        return None;
    }
    Some(parse_mapping_arg(&args[1..]).ok_or(()))
}

fn parse_mapping_arg(args: &[OsString]) -> Option<u64> {
    if args.len() != 1 {
        return None;
    }
    let arg = args[0].to_str()?;
    if arg.is_empty() || arg.len() > 20 || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let raw = arg.parse::<u64>().ok()?;
    super::seven_zip_helper_protocol::valid_raw(raw).then_some(raw)
}

struct View(MEMORY_MAPPED_VIEW_ADDRESS);
impl Drop for View {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.0);
        }
    }
}

struct Admitted {
    dll_path: String,
    output: File,
    items: Vec<super::seven_zip_sdk::Item>,
}

/// 親側 launch builder に渡す manifest の一行。
pub(crate) struct SevenZipHelperEntry<'a> {
    pub(crate) name: String,
    pub(crate) directory: bool,
    pub(crate) expected_length: u64,
    pub(crate) source: Option<&'a File>,
}

/// helper process の Job drain 完了まで必要な capability をまとめて保持する。
pub(crate) struct SevenZipHelperLaunch {
    provider: super::Archive7zCreationLease,
    image: super::application_image::ApplicationImageLease,
    _mapping: super::seven_zip_helper_protocol::FinalizedMapping,
    mapping_duplicate: OwnedHandle,
    file_duplicates: Vec<OwnedHandle>,
    args: Vec<OsString>,
}

impl SevenZipHelperLaunch {
    pub(super) fn args(&self) -> &[OsString] {
        &self.args
    }
    pub(super) fn inherited_handles(&self) -> Vec<HANDLE> {
        std::iter::once(HANDLE(self.mapping_duplicate.as_raw_handle().cast()))
            .chain(
                self.file_duplicates
                    .iter()
                    .map(|h| HANDLE(h.as_raw_handle().cast())),
            )
            .collect()
    }
    pub(super) fn image(&self) -> &super::application_image::ApplicationImageLease {
        &self.image
    }
    pub(super) fn provider(&self) -> &super::Archive7zCreationLease {
        &self.provider
    }

    #[cfg(test)]
    pub(super) fn descriptor_for_test(
        &self,
    ) -> Result<
        super::seven_zip_helper_protocol::Descriptor,
        super::seven_zip_helper_protocol::ProtocolError,
    > {
        self._mapping.read_descriptor()
    }
}

/// 既に admission 済みの worker に渡す継承 handle 一式を作る。
///
/// 呼び出し側 worker は source を独立に開き、保持・hash・seek reset する。
/// route guard、CREATE_NEW、CandidateOwned ACK は worker が確認する。
/// 元 File は呼び出し側が保持し、Job drain 完了まで I/O で共有位置を動かさない。
pub(crate) fn build_helper_launch(
    provider: super::Archive7zCreationLease,
    image: super::application_image::ApplicationImageLease,
    output: &File,
    entries: &[SevenZipHelperEntry<'_>],
) -> Option<SevenZipHelperLaunch> {
    use super::seven_zip_helper_protocol::{
        Descriptor, EntryDescriptor, HandleDescriptor, HandleRole,
    };
    if entries.is_empty() || entries.len() > super::seven_zip_sdk::MAX_ITEMS {
        return None;
    }
    let mut name_bytes = 0usize;
    for entry in entries {
        if entry.name.encode_utf16().count() > super::seven_zip_sdk::MAX_NAME_UNITS {
            return None;
        }
        name_bytes = name_bytes.checked_add(entry.name.len())?;
        if name_bytes > super::seven_zip_sdk::MAX_TOTAL_NAME_BYTES {
            return None;
        }
    }
    let names = entries
        .iter()
        .map(|e| e.name.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    super::seven_zip_sdk::validate_metadata(entries.iter().zip(&names).map(|(e, name)| {
        super::seven_zip_sdk::ItemMetadata {
            name,
            directory: e.directory,
            length: e.expected_length,
            has_source: e.source.is_some(),
        }
    }))
    .ok()?;

    let output_raw = raw_value(HANDLE(output.as_raw_handle().cast()))?;
    let mut source_raws = std::collections::HashSet::new();
    let mut originals = Vec::with_capacity(entries.len() + 1);
    originals.push(HANDLE(output.as_raw_handle().cast()));
    for entry in entries {
        if let Some(source) = entry.source {
            let raw = HANDLE(source.as_raw_handle().cast());
            let value = raw_value(raw)?;
            if value == output_raw || !source_raws.insert(value) {
                return None;
            }
            originals.push(raw);
        }
    }
    if unsafe { GetFileType(HANDLE(output.as_raw_handle().cast())) } != FILE_TYPE_DISK {
        return None;
    }

    let mut file_duplicates = Vec::with_capacity(originals.len());
    let mut handle_descriptors = Vec::with_capacity(originals.len());
    let source_lengths = entries
        .iter()
        .filter_map(|entry| entry.source.map(|_| entry.expected_length))
        .collect::<Vec<_>>();
    let process = unsafe { GetCurrentProcess() };
    for (index, original) in originals.iter().copied().enumerate() {
        let role = if index == 0 {
            HandleRole::Output
        } else {
            HandleRole::Source
        };
        if unsafe { GetFileType(original) } != FILE_TYPE_DISK {
            return None;
        }
        let original_info = file_info(original)?;
        let original_length = if role == HandleRole::Output {
            0
        } else {
            source_lengths.get(index - 1).copied()?
        };
        if !parent_file_matches(
            &original_info,
            is_ntfs(original),
            original_info.dwVolumeSerialNumber,
            file_index(&original_info),
            original_length,
        ) {
            return None;
        }
        let access = if role == HandleRole::Output {
            GENERIC_WRITE.0
        } else {
            GENERIC_READ.0
        } | FILE_READ_ATTRIBUTES.0
            | SYNCHRONIZE.0;
        let mut raw_duplicate = HANDLE::default();
        // SAFETY: 元 File は builder の完了まで借用されており、呼び出し側も共有位置を使わない。
        unsafe {
            DuplicateHandle(
                process,
                original,
                process,
                &mut raw_duplicate,
                access,
                true,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .ok()?;
        }
        // SAFETY: 元が DISK file と確認済みなので、この duplicate は CloseHandle できる。
        if unsafe { GetFileType(raw_duplicate) } != FILE_TYPE_DISK {
            unsafe {
                CloseHandle(raw_duplicate).ok()?;
            }
            return None;
        }
        let duplicate = unsafe { OwnedHandle::from_raw_handle(raw_duplicate.0) };
        let mut flags = 0u32;
        if unsafe { GetHandleInformation(raw_duplicate, &mut flags) }.is_err()
            || flags & HANDLE_FLAG_INHERIT.0 == 0
        {
            return None;
        }
        let info = file_info(raw_duplicate)?;
        let expected_length = if role == HandleRole::Output {
            0
        } else {
            source_lengths.get(index - 1).copied()?
        };
        if !parent_file_matches(
            &info,
            is_ntfs(raw_duplicate),
            original_info.dwVolumeSerialNumber,
            file_index(&original_info),
            expected_length,
        ) {
            return None;
        }
        handle_descriptors.push(HandleDescriptor {
            role,
            raw: raw_value(raw_duplicate)?,
            volume_serial: info.dwVolumeSerialNumber,
            file_index: file_index(&info),
        });
        file_duplicates.push(duplicate);
    }
    let output_identity = (
        handle_descriptors[0].volume_serial,
        handle_descriptors[0].file_index,
    );
    if handle_descriptors
        .iter()
        .skip(1)
        .any(|h| (h.volume_serial, h.file_index) == output_identity)
    {
        return None;
    }

    let mut source_index = 0u32;
    let mut wire_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let source = if entry.source.is_some() {
            source_index += 1;
            Some(source_index)
        } else {
            None
        };
        wire_entries.push(EntryDescriptor {
            directory: entry.directory,
            name: entry.name.clone(),
            source_index: source,
            expected_length: entry.expected_length,
        });
    }
    let dll_path = metadata_dll_path(&provider.0.binding.zip_dll.path)?;
    let descriptor = Descriptor {
        dll_path,
        handles: handle_descriptors,
        entries: wire_entries,
    };
    let frame = super::seven_zip_helper_protocol::encode(&descriptor).ok()?;
    let mapping = super::seven_zip_helper_protocol::create_mapping(&frame).ok()?;
    let process = unsafe { GetCurrentProcess() };
    let mut mapping_raw = HANDLE::default();
    // SAFETY: finalized mapping が所有する section handle から read 権限だけを複製する。
    unsafe {
        DuplicateHandle(
            process,
            HANDLE(mapping.as_handle().as_raw_handle().cast()),
            process,
            &mut mapping_raw,
            SECTION_MAP_READ.0,
            true,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
        .ok()?;
    }
    // SAFETY: mapping_raw は既知の section object を複製した handle。
    let mapping_duplicate = unsafe { OwnedHandle::from_raw_handle(mapping_raw.0) };
    let mut mapping_flags = 0u32;
    if unsafe { GetHandleInformation(mapping_raw, &mut mapping_flags) }.is_err()
        || mapping_flags & HANDLE_FLAG_INHERIT.0 == 0
    {
        return None;
    }
    let raw = raw_value(mapping_raw)?;
    Some(SevenZipHelperLaunch {
        provider,
        image,
        _mapping: mapping,
        mapping_duplicate,
        file_duplicates,
        args: vec![OsString::from(ENTRY_FLAG), OsString::from(raw.to_string())],
    })
}

/// # Safety
/// GPUI 前の process entry から呼び、引数 mapping を admission 中に他 thread が close/reuse しないこと。
unsafe fn run_helper_handle(original_mapping: HANDLE) -> i32 {
    let Some(mapping) = (unsafe { admit_mapping(original_mapping) }) else {
        return 1;
    };
    let cancelled = AtomicBool::new(false);
    if super::seven_zip_sdk::create_from_handles(
        Path::new(&mapping.dll_path),
        mapping.output,
        mapping.items,
        &cancelled,
    )
    .is_ok()
    {
        0
    } else {
        1
    }
}

/// # Safety
/// `original` はこの process の handle table に属し、この関数の完了まで close/reuse しないこと。
unsafe fn admit_mapping(original: HANDLE) -> Option<Admitted> {
    if !valid_inheritable(original) {
        return None;
    }
    // 小さな header view で section object と確認し、未 admission の raw handle は所有しない。
    let view = unsafe {
        MapViewOfFile(
            original,
            FILE_MAP_READ,
            0,
            0,
            super::seven_zip_helper_protocol::HEADER_SIZE,
        )
    };
    if view.Value.is_null() {
        return None;
    }
    let _view = View(view);

    let process = unsafe { GetCurrentProcess() };
    let mut duplicated = HANDLE::default();
    // SAFETY: 元 handle は直前に mapping と確認済みで、admission 完了まで同一 thread が保つ。
    unsafe {
        DuplicateHandle(
            process,
            original,
            process,
            &mut duplicated,
            SECTION_MAP_READ.0,
            false,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
        .ok()?;
    }
    // SAFETY: 元 handle が section object なので、その duplicate も CloseHandle 対象として安全。
    let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicated.0) };
    let descriptor = super::seven_zip_helper_protocol::read_mapping(duplicate.as_handle()).ok()?;
    unsafe { admit_descriptor(descriptor, raw_value(original)?, raw_value(duplicated)?) }
}

/// # Safety
/// descriptor の raw value は未所有 handle 番号としてこの関数へ移管する。
/// admission 中に他 owner が close/reuse せず、成功時の close をこの関数だけが行うこと。
unsafe fn admit_descriptor(
    descriptor: super::seven_zip_helper_protocol::Descriptor,
    original_mapping_raw: u64,
    duplicate_mapping_raw: u64,
) -> Option<Admitted> {
    if original_mapping_raw == duplicate_mapping_raw
        || descriptor.rejects_mapping_handle(original_mapping_raw)
        || descriptor.rejects_mapping_handle(duplicate_mapping_raw)
    {
        return None;
    }

    let names = descriptor
        .entries
        .iter()
        .map(|entry| entry.name.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let metadata = descriptor.entries.iter().zip(&names).map(|(entry, name)| {
        super::seven_zip_sdk::ItemMetadata {
            name,
            directory: entry.directory,
            length: entry.expected_length,
            has_source: entry.source_index.is_some(),
        }
    });
    // File duplicate/変換の前に SDK と同じ manifest 規則を一度だけ通す。
    super::seven_zip_sdk::validate_metadata(metadata).ok()?;

    let process = unsafe { GetCurrentProcess() };
    let mut seen = std::collections::HashSet::with_capacity(descriptor.handles.len());
    let mut originals = Vec::with_capacity(descriptor.handles.len());
    let mut expected_lengths = vec![None; descriptor.handles.len()];
    for entry in &descriptor.entries {
        if let Some(index) = entry.source_index {
            let slot = expected_lengths.get_mut(index as usize)?;
            if slot.replace(entry.expected_length).is_some() {
                return None;
            }
        }
    }
    // duplicate が後続の未検証 raw 番号を再利用しないよう、全 table を先に preflight する。
    for handle in &descriptor.handles {
        let raw = handle.raw;
        if !super::seven_zip_helper_protocol::valid_raw(raw)
            || raw == original_mapping_raw
            || raw == duplicate_mapping_raw
            || !seen.insert(raw)
        {
            return None;
        }
        let original = handle_from_raw(raw)?;
        if !valid_inheritable(original) || unsafe { GetFileType(original) } != FILE_TYPE_DISK {
            return None;
        }
        originals.push(original);
    }

    let mut duplicates = Vec::<OwnedHandle>::with_capacity(descriptor.handles.len());
    let mut identities = Vec::with_capacity(descriptor.handles.len());
    for (index, handle) in descriptor.handles.iter().enumerate() {
        let original = originals[index];
        let requested = match handle.role {
            super::seven_zip_helper_protocol::HandleRole::Output => {
                GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0
            }
            super::seven_zip_helper_protocol::HandleRole::Source => {
                GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0
            }
        };
        let mut raw_duplicate = HANDLE::default();
        // SAFETY: 元 handle は安定した DISK file と確認済み。要求権限を必要最小限にする。
        unsafe {
            DuplicateHandle(
                process,
                original,
                process,
                &mut raw_duplicate,
                requested,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .ok()?;
        }
        // SAFETY: stable な DISK file から作ったため、duplicate も CloseHandle で閉じられる。
        let duplicate = unsafe { OwnedHandle::from_raw_handle(raw_duplicate.0) };
        if unsafe { GetFileType(raw_duplicate) } != FILE_TYPE_DISK {
            return None;
        }
        let info = file_info(raw_duplicate)?;
        if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
            != 0
            || !is_ntfs(raw_duplicate)
            || info.dwVolumeSerialNumber != handle.volume_serial
            || file_index(&info) != handle.file_index
        {
            return None;
        }
        let length = file_length(&info);
        let expected = expected_lengths[index].unwrap_or(0);
        if length != expected {
            return None;
        }
        identities.push((info.dwVolumeSerialNumber, file_index(&info)));
        duplicates.push(duplicate);
    }
    let output_identity = *identities.first()?;
    if identities
        .iter()
        .skip(1)
        .any(|identity| *identity == output_identity)
    {
        return None;
    }

    // 全 duplicate の検証後、preflight 済みの元 file handle だけを一度ずつ閉じる。
    for original in originals {
        unsafe {
            CloseHandle(original).ok()?;
        }
    }

    let mut files = duplicates
        .into_iter()
        .map(|handle| unsafe { File::from_raw_handle(handle.into_raw_handle()) })
        .collect::<Vec<_>>();
    for file in &mut files {
        file.seek(SeekFrom::Start(0)).ok()?;
    }
    let mut sources = files.drain(1..).map(Some).collect::<Vec<_>>();
    let output = files.pop()?;
    let mut items = Vec::with_capacity(descriptor.entries.len());
    for (entry, name) in descriptor.entries.into_iter().zip(names) {
        let source = match entry.source_index {
            Some(index) => sources.get_mut(index as usize - 1)?.take(),
            None => None,
        };
        items.push(super::seven_zip_sdk::Item {
            name,
            directory: entry.directory,
            length: entry.expected_length,
            source,
        });
    }
    Some(Admitted {
        dll_path: descriptor.dll_path,
        output,
        items,
    })
}

fn raw_value(handle: HANDLE) -> Option<u64> {
    let raw = handle.0 as usize as u64;
    super::seven_zip_helper_protocol::valid_raw(raw).then_some(raw)
}

fn handle_from_raw(raw: u64) -> Option<HANDLE> {
    if !super::seven_zip_helper_protocol::valid_raw(raw) {
        return None;
    }
    Some(HANDLE(raw as usize as *mut c_void))
}

fn valid_inheritable(handle: HANDLE) -> bool {
    let mut flags = 0u32;
    unsafe { GetHandleInformation(handle, &mut flags) }.is_ok()
        && flags & HANDLE_FLAG_INHERIT.0 != 0
}

fn file_info(handle: HANDLE) -> Option<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info).ok()? };
    Some(info)
}

fn file_index(info: &BY_HANDLE_FILE_INFORMATION) -> u64 {
    (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
}

fn file_length(info: &BY_HANDLE_FILE_INFORMATION) -> u64 {
    (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow)
}

fn metadata_dll_path(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

fn parent_file_matches(
    info: &BY_HANDLE_FILE_INFORMATION,
    ntfs: bool,
    expected_volume: u32,
    expected_index: u64,
    expected_length: u64,
) -> bool {
    ntfs && info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
        == 0
        && info.dwVolumeSerialNumber == expected_volume
        && file_index(info) == expected_index
        && file_length(info) == expected_length
}

fn is_ntfs(handle: HANDLE) -> bool {
    let mut filesystem = [0u16; 64];
    unsafe { GetVolumeInformationByHandleW(handle, None, None, None, None, Some(&mut filesystem)) }
        .is_ok()
        && String::from_utf16_lossy(&filesystem)
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("NTFS")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::{
        fs::{self, OpenOptions},
        io::{Read, Write},
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
    };
    use windows::Win32::{
        Foundation::{
            DUPLICATE_HANDLE_OPTIONS, GENERIC_READ, GENERIC_WRITE, HANDLE_FLAGS,
            SetHandleInformation,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile},
        System::Threading::{CreateEventW, SetEvent},
    };
    use windows::core::PCWSTR;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    pub(super) fn parent_leases_for_app(
        app: &Path,
    ) -> (
        tempfile::TempDir,
        super::super::Archive7zCreationLease,
        super::super::application_image::ApplicationImageLease,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("7z.exe");
        let dll = directory.path().join("7z.dll");
        fs::write(&executable, b"test executable").unwrap();
        fs::write(&dll, b"test dll").unwrap();
        let binding = super::super::ArchiveOperationBinding {
            executable: super::super::windows_probe::test_file_hint(&executable),
            zip_dll: super::super::windows_probe::test_file_hint(&dll),
        };
        let owner = super::super::ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        assert!(owner.commit_discovery(
            revision,
            Some(super::super::SevenZipExecutable {
                path: executable,
                execution_hints: super::super::ArchiveExecutionHints {
                    candidate_executable: None,
                    loaded_zip_dll: Some(binding.zip_dll.clone()),
                    operation_binding: Some(binding.clone()),
                },
                version: None,
                formats: Some(vec![super::super::SevenZipFormat {
                    name: "7z".into(),
                    flags: "C........".into(),
                }]),
            })
        ));
        let provider = owner.lease_7z_creation(revision, &binding).unwrap();
        let image = super::super::application_image::lease_explicit_path_for_test(app).unwrap();
        (directory, provider, image)
    }

    fn parent_leases() -> (
        tempfile::TempDir,
        super::super::Archive7zCreationLease,
        super::super::application_image::ApplicationImageLease,
    ) {
        parent_leases_for_app(&std::env::current_exe().unwrap())
    }

    fn duplicate_access(handle: HANDLE, access: u32) -> windows::core::Result<HANDLE> {
        let process = unsafe { GetCurrentProcess() };
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateHandle(
                process,
                handle,
                process,
                &mut duplicate,
                access,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )?;
        }
        Ok(duplicate)
    }

    fn assert_parent_file_open(file: &File) {
        let handle = HANDLE(file.as_raw_handle().cast());
        assert_eq!(unsafe { GetFileType(handle) }, FILE_TYPE_DISK);
        assert!(file.metadata().is_ok());
    }

    fn assert_duplicate_denied(handle: HANDLE, access: u32) {
        match duplicate_access(handle, access) {
            Ok(duplicate) => {
                unsafe { CloseHandle(duplicate).unwrap() };
                panic!("handle unexpectedly granted requested access {access:#x}");
            }
            Err(_) => {}
        }
    }

    #[test]
    fn helper_entry_parser_distinguishes_normal_and_malformed_invocations() {
        assert_eq!(classify_helper_args(&args(&["--demo"])), None);
        assert_eq!(classify_helper_args(&args(&[])), None);
        assert_eq!(classify_helper_args(&args(&[ENTRY_FLAG])), Some(Err(())));
        assert_eq!(
            classify_helper_args(&args(&[ENTRY_FLAG, "not-a-number"])),
            Some(Err(()))
        );
        assert_eq!(
            classify_helper_args(&args(&[ENTRY_FLAG, "42", "43"])),
            Some(Err(()))
        );
        assert_eq!(
            classify_helper_args(&args(&[ENTRY_FLAG, "42"])),
            Some(Ok(42))
        );
        assert_eq!(parse_mapping_arg(&args(&["42"])), Some(42));
        assert_eq!(parse_mapping_arg(&args(&["0"])), None);
        assert_eq!(parse_mapping_arg(&args(&["+42"])), None);
        assert_eq!(parse_mapping_arg(&args(&[" 42"])), None);
        assert_eq!(parse_mapping_arg(&args(&["-1"])), None);
        assert_eq!(parse_mapping_arg(&args(&["４２"])), None);
        assert_eq!(
            parse_mapping_arg(&args(&["00000000000000000042"])),
            Some(42)
        );
        assert_eq!(parse_mapping_arg(&args(&["000000000000000000042"])), None);
        assert_eq!(parse_mapping_arg(&args(&["18446744073709551616"])), None);
        assert_eq!(parse_mapping_arg(&args(&["18446744073709551615"])), None);
        assert_eq!(parse_mapping_arg(&args(&[])), None);
        assert_eq!(parse_mapping_arg(&args(&["42", "43"])), None);
    }

    #[test]
    fn helper_dll_path_encoding_fails_closed_for_non_unicode_paths() {
        use std::os::windows::ffi::OsStringExt;
        let path = std::path::PathBuf::from(OsString::from_wide(&[0xd800]));
        assert!(metadata_dll_path(&path).is_none());
        assert_eq!(
            metadata_dll_path(Path::new(r"C:\Program Files\7-Zip\7z.dll")),
            Some(r"C:\Program Files\7-Zip\7z.dll".into())
        );
    }

    struct RawHandles {
        handles: Vec<HANDLE>,
        closed_by_admission: bool,
    }

    impl RawHandles {
        fn new() -> Self {
            Self {
                handles: Vec::new(),
                closed_by_admission: false,
            }
        }
        fn push(&mut self, handle: HANDLE) {
            self.handles.push(handle);
        }
        fn admission_succeeded(&mut self) {
            self.closed_by_admission = true;
        }
        fn assert_open(&self) {
            for handle in &self.handles {
                let mut flags = 0;
                assert!(unsafe { GetHandleInformation(*handle, &mut flags) }.is_ok());
            }
        }
        fn assert_closed(&self) {
            for handle in &self.handles {
                let mut flags = 0;
                assert!(unsafe { GetHandleInformation(*handle, &mut flags) }.is_err());
            }
        }
    }

    impl Drop for RawHandles {
        fn drop(&mut self) {
            if self.closed_by_admission {
                return;
            }
            let mut unique = std::collections::HashSet::new();
            for handle in &self.handles {
                let raw = handle.0 as usize;
                if unique.insert(raw) {
                    unsafe {
                        let _ = CloseHandle(*handle);
                    }
                }
            }
        }
    }

    fn duplicate_for_admission(handle: HANDLE, inheritable: bool) -> HANDLE {
        let process = unsafe { GetCurrentProcess() };
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateHandle(
                process,
                handle,
                process,
                &mut duplicate,
                0,
                inheritable,
                DUPLICATE_HANDLE_OPTIONS(2),
            )
            .unwrap();
        }
        duplicate
    }

    fn file_identity(handle: HANDLE) -> (u32, u64) {
        let info = file_info(handle).unwrap();
        (info.dwVolumeSerialNumber, file_index(&info))
    }

    fn descriptor(
        output: HANDLE,
        sources: &[HANDLE],
        source_paths: &[&str],
        lengths: &[u64],
    ) -> super::super::seven_zip_helper_protocol::Descriptor {
        use super::super::seven_zip_helper_protocol::{
            Descriptor, EntryDescriptor, HandleDescriptor, HandleRole,
        };
        let (output_volume, output_index) = file_identity(output);
        let mut handles = vec![HandleDescriptor {
            role: HandleRole::Output,
            raw: output.0 as usize as u64,
            volume_serial: output_volume,
            file_index: output_index,
        }];
        for source in sources {
            let (volume_serial, file_index) = file_identity(*source);
            handles.push(HandleDescriptor {
                role: HandleRole::Source,
                raw: source.0 as usize as u64,
                volume_serial,
                file_index,
            });
        }
        let mut entries = Vec::new();
        for (index, (path, length)) in source_paths.iter().zip(lengths).enumerate() {
            entries.push(EntryDescriptor {
                directory: false,
                name: (*path).into(),
                source_index: Some(index as u32 + 1),
                expected_length: *length,
            });
        }
        Descriptor {
            dll_path: r"C:\Windows\System32\7z.dll".into(),
            handles,
            entries,
        }
    }

    fn open_file(path: &std::path::Path, write: bool) -> File {
        OpenOptions::new()
            .read(!write)
            .write(write)
            .open(path)
            .unwrap()
    }

    fn inheritable_file(file: &File, owner: &mut RawHandles, inheritable: bool) -> HANDLE {
        let duplicate = duplicate_for_admission(HANDLE(file.as_raw_handle().cast()), inheritable);
        owner.push(duplicate);
        duplicate
    }

    fn restricted_inheritable_file(
        path: &std::path::Path,
        access: u32,
        owner: &mut RawHandles,
    ) -> HANDLE {
        let path = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .unwrap();
        unsafe {
            SetHandleInformation(
                handle,
                HANDLE_FLAG_INHERIT.0,
                HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0),
            )
            .unwrap();
        }
        owner.push(handle);
        handle
    }

    fn open_restricted_file(path: &std::path::Path, access: u32) -> File {
        let path = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .unwrap();
        unsafe { File::from_raw_handle(handle.0) }
    }

    fn mapping_ids() -> (u64, u64) {
        // These are comparison-only values passed to the descriptor admission core.
        (1, 2)
    }

    fn admit(desc: super::super::seven_zip_helper_protocol::Descriptor) -> Option<Admitted> {
        let (original, duplicate) = mapping_ids();
        unsafe { admit_descriptor(desc, original, duplicate) }
    }

    fn temp_pair(bytes: &[u8]) -> (tempfile::TempDir, File, File) {
        let directory = tempfile::tempdir().unwrap();
        let output_path = directory.path().join("output.7z");
        let source_path = directory.path().join("source.bin");
        fs::write(&source_path, bytes).unwrap();
        fs::write(&output_path, []).unwrap();
        let output = OpenOptions::new().write(true).open(&output_path).unwrap();
        let source = open_file(&source_path, false);
        (directory, output, source)
    }

    #[test]
    fn mapping_entry_rejects_event_and_noninheritable_mapping_without_owning_input() {
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.unwrap();
        let event = event;
        unsafe {
            SetHandleInformation(
                event,
                HANDLE_FLAG_INHERIT.0,
                HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0),
            )
            .unwrap();
        }
        assert!(unsafe { admit_mapping(event) }.is_none());
        assert!(unsafe { SetEvent(event) }.is_ok());
        unsafe {
            let _ = CloseHandle(event);
        }

        let raw = super::super::seven_zip_helper_protocol::Descriptor {
            dll_path: r"C:\Windows\System32\7z.dll".into(),
            handles: vec![super::super::seven_zip_helper_protocol::HandleDescriptor {
                role: super::super::seven_zip_helper_protocol::HandleRole::Output,
                raw: 9,
                volume_serial: 1,
                file_index: 1,
            }],
            entries: vec![super::super::seven_zip_helper_protocol::EntryDescriptor {
                directory: true,
                name: "empty".into(),
                source_index: None,
                expected_length: 0,
            }],
        };
        let frame = super::super::seven_zip_helper_protocol::encode(&raw).unwrap();
        let mapping = super::super::seven_zip_helper_protocol::create_mapping(&frame).unwrap();
        let mapping_handle = HANDLE(mapping.as_handle().as_raw_handle().cast());
        assert!(unsafe { admit_mapping(mapping_handle) }.is_none());
        assert!(mapping.read_descriptor().is_ok());
    }

    #[test]
    fn whole_original_table_preflight_rejects_wrong_kind_noninherit_free_and_duplicate_values() {
        let (_temp, output, source) = temp_pair(&[]);
        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let mut bad = descriptor(output_handle, &[], &[], &[]);

        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.unwrap();
        unsafe {
            SetHandleInformation(
                event,
                HANDLE_FLAG_INHERIT.0,
                HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0),
            )
            .unwrap();
        }
        owner.push(event);
        let (volume, index) = (0, 0);
        bad.handles
            .push(super::super::seven_zip_helper_protocol::HandleDescriptor {
                role: super::super::seven_zip_helper_protocol::HandleRole::Source,
                raw: event.0 as usize as u64,
                volume_serial: volume,
                file_index: index,
            });
        bad.entries = vec![super::super::seven_zip_helper_protocol::EntryDescriptor {
            directory: false,
            name: "wrong-kind".into(),
            source_index: Some(1),
            expected_length: 0,
        }];
        assert!(admit(bad).is_none());
        owner.assert_open();
        assert!(unsafe { SetEvent(event) }.is_ok());

        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let source_handle = inheritable_file(&source, &mut owner, false);
        let mut bad = descriptor(output_handle, &[source_handle], &["file"], &[0]);
        assert!(admit(bad.clone()).is_none());
        owner.assert_open();

        bad.handles[1].raw = u64::MAX - 6;
        assert!(admit(bad).is_none());
        owner.assert_open();

        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let source_handle = inheritable_file(&source, &mut owner, true);
        let mut bad = descriptor(output_handle, &[source_handle], &["file"], &[0]);
        bad.handles[1].raw = bad.handles[0].raw;
        assert!(admit(bad).is_none());
        owner.assert_open();
    }

    #[test]
    fn mapping_collisions_and_identity_or_length_mismatches_leave_originals_open() {
        let (_temp, output, source) = temp_pair(b"source");
        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let source_handle = inheritable_file(&source, &mut owner, true);
        let good = descriptor(output_handle, &[source_handle], &["file"], &[6]);

        let mut collision = good.clone();
        let (mapping_original, mapping_duplicate) = mapping_ids();
        collision.handles[1].raw = mapping_original;
        assert!(
            unsafe { admit_descriptor(collision, mapping_original, mapping_duplicate) }.is_none()
        );
        owner.assert_open();
        let mut collision = good.clone();
        collision.handles[1].raw = mapping_duplicate;
        assert!(
            unsafe { admit_descriptor(collision, mapping_original, mapping_duplicate) }.is_none()
        );
        owner.assert_open();

        let mut wrong_identity = good.clone();
        wrong_identity.handles[1].file_index = wrong_identity.handles[1].file_index.wrapping_add(1);
        assert!(admit(wrong_identity).is_none());
        owner.assert_open();
        let mut wrong_length = good;
        wrong_length.entries[0].expected_length = 5;
        assert!(admit(wrong_length).is_none());
        owner.assert_open();
    }

    #[test]
    fn output_source_identity_alias_is_rejected_even_when_both_are_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("same.bin");
        fs::write(&path, []).unwrap();
        let output = open_file(&path, true);
        let source = open_file(&path, false);
        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let source_handle = inheritable_file(&source, &mut owner, true);
        let bad = descriptor(output_handle, &[source_handle], &["empty"], &[0]);
        assert!(admit(bad).is_none());
        owner.assert_open();
    }

    #[test]
    fn admission_rejects_sources_without_read_data_and_outputs_without_write_data_rights() {
        let directory = tempfile::tempdir().unwrap();
        let output_path = directory.path().join("output.7z");
        let source_path = directory.path().join("source.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_path, []).unwrap();
        let output = open_file(&output_path, true);
        let source = open_file(&source_path, false);
        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let restricted_source = restricted_inheritable_file(
            &source_path,
            FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
            &mut owner,
        );
        let missing_read = descriptor(output_handle, &[restricted_source], &["empty"], &[0]);
        assert!(admit(missing_read).is_none());
        owner.assert_open();

        let mut owner = RawHandles::new();
        let restricted_output = restricted_inheritable_file(
            &output_path,
            FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0,
            &mut owner,
        );
        let source_handle = inheritable_file(&source, &mut owner, true);
        let missing_write = descriptor(restricted_output, &[source_handle], &["empty"], &[0]);
        assert!(admit(missing_write).is_none());
        owner.assert_open();
    }

    #[test]
    fn admitted_files_are_narrow_and_distinct_hardlink_sources_are_allowed() {
        let directory = tempfile::tempdir().unwrap();
        let output_path = directory.path().join("output.7z");
        let source_a_path = directory.path().join("a.bin");
        let source_b_path = directory.path().join("b.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_a_path, []).unwrap();
        fs::hard_link(&source_a_path, &source_b_path).unwrap();
        let output = open_file(&output_path, true);
        let source_a = open_file(&source_a_path, false);
        let source_b = open_file(&source_b_path, false);
        let mut owner = RawHandles::new();
        let output_handle = inheritable_file(&output, &mut owner, true);
        let source_a_handle = inheritable_file(&source_a, &mut owner, true);
        let source_b_handle = inheritable_file(&source_b, &mut owner, true);
        let mut desc = descriptor(
            output_handle,
            &[source_a_handle, source_b_handle],
            &["日本語/@-a", "empty-b"],
            &[0, 0],
        );
        desc.entries.insert(
            0,
            super::super::seven_zip_helper_protocol::EntryDescriptor {
                directory: true,
                name: "empty-directory".into(),
                source_index: None,
                expected_length: 0,
            },
        );
        let frame = super::super::seven_zip_helper_protocol::encode(&desc).unwrap();
        let mapping = super::super::seven_zip_helper_protocol::create_mapping(&frame).unwrap();
        let inherited_mapping =
            duplicate_for_admission(HANDLE(mapping.as_handle().as_raw_handle().cast()), true);
        let mut mapping_owner = RawHandles::new();
        mapping_owner.push(inherited_mapping);
        let admitted = unsafe { admit_mapping(inherited_mapping) }
            .expect("distinct hardlink sources and a valid mapping are accepted");
        owner.admission_succeeded();
        owner.assert_closed();
        mapping_owner.assert_open();
        assert_eq!(admitted.items.len(), 3);
        assert!(admitted.items[0].directory);
        assert!(admitted.items[1].source.is_some());
        assert!(admitted.items[2].source.is_some());
        let mut input = admitted.items[1].source.as_ref().unwrap();
        assert!(input.write(&[1]).is_err());
        let mut output = admitted.output;
        assert!(output.read(&mut [0]).is_err());
    }

    #[test]
    fn parent_builder_owns_narrow_inheritable_duplicates_until_bundle_drop() {
        let root = tempfile::tempdir().unwrap();
        let output_path = root.path().join("candidate.7z");
        let source_a_path = root.path().join("a.bin");
        let source_b_path = root.path().join("b.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_a_path, []).unwrap();
        fs::hard_link(&source_a_path, &source_b_path).unwrap();
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output_path)
            .unwrap();
        let source_a = open_file(&source_a_path, false);
        let source_b = open_file(&source_b_path, false);
        let entries = [
            SevenZipHelperEntry {
                name: "日本語/@-a".into(),
                directory: false,
                expected_length: 0,
                source: Some(&source_a),
            },
            SevenZipHelperEntry {
                name: "b".into(),
                directory: false,
                expected_length: 0,
                source: Some(&source_b),
            },
            SevenZipHelperEntry {
                name: "empty-dir".into(),
                directory: true,
                expected_length: 0,
                source: None,
            },
        ];
        let (_provider_root, provider, image) = parent_leases();
        let launch = build_helper_launch(provider, image, &output, &entries).unwrap();
        let inherited = launch.inherited_handles();
        assert_eq!(inherited.len(), 4);
        let mut flags = 0;
        assert!(unsafe { GetHandleInformation(inherited[0], &mut flags) }.is_ok());
        assert_ne!(flags & HANDLE_FLAG_INHERIT.0, 0);
        flags = 0;
        let original_mapping = HANDLE(launch._mapping.as_handle().as_raw_handle().cast());
        assert!(unsafe { GetHandleInformation(original_mapping, &mut flags) }.is_ok());
        assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0);
        let read_view = unsafe {
            MapViewOfFile(
                inherited[0],
                FILE_MAP_READ,
                0,
                0,
                super::super::seven_zip_helper_protocol::HEADER_SIZE,
            )
        };
        assert!(!read_view.Value.is_null());
        unsafe { UnmapViewOfFile(read_view).unwrap() };
        assert!(
            unsafe {
                MapViewOfFile(
                    inherited[0],
                    FILE_MAP_WRITE,
                    0,
                    0,
                    super::super::seven_zip_helper_protocol::HEADER_SIZE,
                )
            }
            .Value
            .is_null()
        );
        assert_duplicate_denied(inherited[1], GENERIC_READ.0);
        assert_duplicate_denied(inherited[1], 0x0001_0000);
        assert_duplicate_denied(inherited[2], GENERIC_WRITE.0);
        assert_duplicate_denied(inherited[2], 0x0001_0000);
        assert_duplicate_denied(inherited[3], GENERIC_WRITE.0);
        assert_eq!(
            file_identity(inherited[2]),
            file_identity(inherited[3]),
            "independently opened hardlink sources may share identity"
        );
        drop(launch);
        for handle in inherited {
            let mut flags = 0;
            assert!(unsafe { GetHandleInformation(handle, &mut flags) }.is_err());
        }
        assert_parent_file_open(&output);
        assert_parent_file_open(&source_a);
        assert_parent_file_open(&source_b);
    }

    #[test]
    fn parent_builder_rejects_metadata_association_alias_length_and_output_failures() {
        let root = tempfile::tempdir().unwrap();
        let output_path = root.path().join("candidate.7z");
        let source_path = root.path().join("source.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_path, []).unwrap();
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output_path)
            .unwrap();
        let source = open_file(&source_path, false);

        let invalid_sets = [
            vec![SevenZipHelperEntry {
                name: "../bad".into(),
                directory: false,
                expected_length: 0,
                source: Some(&source),
            }],
            vec![SevenZipHelperEntry {
                name: "missing".into(),
                directory: false,
                expected_length: 0,
                source: None,
            }],
            vec![SevenZipHelperEntry {
                name: "unexpected-dir-source".into(),
                directory: true,
                expected_length: 0,
                source: Some(&source),
            }],
            vec![SevenZipHelperEntry {
                name: "wrong-length".into(),
                directory: false,
                expected_length: 1,
                source: Some(&source),
            }],
            vec![
                SevenZipHelperEntry {
                    name: "repeat-a".into(),
                    directory: false,
                    expected_length: 0,
                    source: Some(&source),
                },
                SevenZipHelperEntry {
                    name: "repeat-b".into(),
                    directory: false,
                    expected_length: 0,
                    source: Some(&source),
                },
            ],
        ];
        for entries in invalid_sets {
            let (_provider_root, provider, image) = parent_leases();
            assert!(build_helper_launch(provider, image, &output, &entries).is_none());
            assert_parent_file_open(&output);
            assert_parent_file_open(&source);
        }

        let too_many = (0..=super::super::seven_zip_sdk::MAX_ITEMS)
            .map(|index| SevenZipHelperEntry {
                name: format!("dir-{index}"),
                directory: true,
                expected_length: 0,
                source: None,
            })
            .collect::<Vec<_>>();
        let (_provider_root, provider, image) = parent_leases();
        assert!(build_helper_launch(provider, image, &output, &too_many).is_none());

        let hardlink_path = root.path().join("candidate-alias.bin");
        fs::hard_link(&source_path, &hardlink_path).unwrap();
        let alias_output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&hardlink_path)
            .unwrap();
        let alias_entry = [SevenZipHelperEntry {
            name: "empty".into(),
            directory: false,
            expected_length: 0,
            source: Some(&source),
        }];
        let (_provider_root, provider, image) = parent_leases();
        assert!(build_helper_launch(provider, image, &alias_output, &alias_entry).is_none());

        fs::write(&output_path, b"already nonempty").unwrap();
        let (_provider_root, provider, image) = parent_leases();
        let valid_entry = [SevenZipHelperEntry {
            name: "file".into(),
            directory: false,
            expected_length: 0,
            source: Some(&source),
        }];
        assert!(build_helper_launch(provider, image, &output, &valid_entry).is_none());
        assert!(output.metadata().unwrap().len() > 0);
    }

    #[test]
    fn parent_builder_rejects_original_handles_without_requested_read_or_write_rights() {
        let root = tempfile::tempdir().unwrap();
        let output_path = root.path().join("output.7z");
        let source_path = root.path().join("source.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_path, []).unwrap();
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output_path)
            .unwrap();
        let source_without_read =
            open_restricted_file(&source_path, FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0);
        let source_entry = [SevenZipHelperEntry {
            name: "source".into(),
            directory: false,
            expected_length: 0,
            source: Some(&source_without_read),
        }];
        let (_provider_root, provider, image) = parent_leases();
        assert!(build_helper_launch(provider, image, &output, &source_entry).is_none());

        let output_without_write =
            open_restricted_file(&output_path, FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0);
        let source = open_file(&source_path, false);
        let source_entry = [SevenZipHelperEntry {
            name: "source".into(),
            directory: false,
            expected_length: 0,
            source: Some(&source),
        }];
        let (_provider_root, provider, image) = parent_leases();
        assert!(
            build_helper_launch(provider, image, &output_without_write, &source_entry).is_none()
        );
        assert_parent_file_open(&output);
        assert_parent_file_open(&source);
    }

    #[test]
    fn parent_file_domain_gate_rejects_non_ntfs_and_directories() {
        let root = tempfile::tempdir().unwrap();
        let dir_handle = unsafe {
            let path = root
                .path()
                .as_os_str()
                .encode_wide()
                .chain([0])
                .collect::<Vec<_>>();
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .unwrap();
        let directory_file = unsafe { File::from_raw_handle(dir_handle.0) };
        let directory_info = file_info(HANDLE(directory_file.as_raw_handle().cast())).unwrap();
        assert!(!parent_file_matches(
            &directory_info,
            true,
            directory_info.dwVolumeSerialNumber,
            file_index(&directory_info),
            0,
        ));

        let file_path = root.path().join("domain.bin");
        fs::write(&file_path, []).unwrap();
        let file = open_file(&file_path, false);
        let info = file_info(HANDLE(file.as_raw_handle().cast())).unwrap();
        assert!(parent_file_matches(
            &info,
            is_ntfs(HANDLE(file.as_raw_handle().cast())),
            info.dwVolumeSerialNumber,
            file_index(&info),
            0,
        ));
        assert!(!parent_file_matches(
            &info,
            false,
            info.dwVolumeSerialNumber,
            file_index(&info),
            0,
        ));

        let output_path = root.path().join("directory-check-output.7z");
        fs::write(&output_path, []).unwrap();
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output_path)
            .unwrap();
        let entry = [SevenZipHelperEntry {
            name: "directory-as-source".into(),
            directory: false,
            expected_length: 0,
            source: Some(&directory_file),
        }];
        let (_provider_root, provider, image) = parent_leases();
        assert!(build_helper_launch(provider, image, &output, &entry).is_none());

        let ordinary_path = root.path().join("ordinary.bin");
        let link_path = root.path().join("reparse.bin");
        fs::write(&ordinary_path, []).unwrap();
        let symlink_result = std::os::windows::fs::symlink_file(&ordinary_path, &link_path);
        if symlink_result.is_ok() {
            let wide = link_path
                .as_os_str()
                .encode_wide()
                .chain([0])
                .collect::<Vec<_>>();
            let reparse_handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    None,
                )
            }
            .unwrap();
            let reparse_file = unsafe { File::from_raw_handle(reparse_handle.0) };
            let entry = [SevenZipHelperEntry {
                name: "reparse-source".into(),
                directory: false,
                expected_length: 0,
                source: Some(&reparse_file),
            }];
            let (_provider_root, provider, image) = parent_leases();
            assert!(build_helper_launch(provider, image, &output, &entry).is_none());
            eprintln!("reparse source rejection exercised");
        } else {
            eprintln!(
                "reparse source case unavailable: {:?}",
                symlink_result.err()
            );
        }
    }

    #[test]
    #[ignore = "requires GPUI_FILER_NON_NTFS_TEST_ROOT on a writable non-NTFS local volume"]
    fn parent_builder_rejects_real_non_ntfs_source_and_output_handles() {
        let non_ntfs_root = std::env::var_os("GPUI_FILER_NON_NTFS_TEST_ROOT")
            .expect("set GPUI_FILER_NON_NTFS_TEST_ROOT to a writable non-NTFS directory");
        let non_ntfs_root = Path::new(&non_ntfs_root);
        assert!(non_ntfs_root.is_absolute() && non_ntfs_root.is_dir());
        let root = tempfile::tempdir_in(non_ntfs_root).unwrap();
        let output_path = root.path().join("candidate.7z");
        let source_path = root.path().join("source.bin");
        fs::write(&output_path, []).unwrap();
        fs::write(&source_path, []).unwrap();
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output_path)
            .unwrap();
        let source = open_file(&source_path, false);
        assert!(!is_ntfs(HANDLE(output.as_raw_handle().cast())));
        let entries = [SevenZipHelperEntry {
            name: "source".into(),
            directory: false,
            expected_length: 0,
            source: Some(&source),
        }];
        let (_provider_root, provider, image) = parent_leases();
        assert!(build_helper_launch(provider, image, &output, &entries).is_none());
        assert_parent_file_open(&output);
        assert_parent_file_open(&source);

        // 出力側で先に拒否されないよう、入力だけが非 NTFS の場合も通す。
        let ntfs_root = tempfile::tempdir().unwrap();
        let ntfs_output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(ntfs_root.path().join("candidate.7z"))
            .unwrap();
        assert!(is_ntfs(HANDLE(ntfs_output.as_raw_handle().cast())));
        assert!(!is_ntfs(HANDLE(source.as_raw_handle().cast())));
        let (_provider_root, provider, image) = parent_leases();
        assert!(build_helper_launch(provider, image, &ntfs_output, &entries).is_none());
        assert_parent_file_open(&ntfs_output);
        assert_parent_file_open(&source);
    }
}
