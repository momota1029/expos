use super::verify::{self, EntryKind, ManifestEntry};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Seek, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            BOOL, CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, GENERIC_READ,
            GENERIC_WRITE, HANDLE,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        },
        System::Threading::{GetCurrentProcess, SYNCHRONIZATION_SYNCHRONIZE},
    },
    core::PCWSTR,
};

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn wide(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain([0]).collect()
}

fn duplicate_inheritable(source: HANDLE, access: u32) -> OwnedHandle {
    let mut duplicate = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &mut duplicate,
            access,
            BOOL(1),
            DUPLICATE_HANDLE_OPTIONS(0),
        )
        .unwrap();
    }
    OwnedHandle(duplicate)
}

fn create_new_candidate(path: &Path) -> File {
    let path = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0 | 0x0001_0000,
            FILE_SHARE_READ,
            None,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .unwrap();
    unsafe { File::from_raw_handle(handle.0) }
}

fn open_source(path: &Path) -> File {
    OpenOptions::new().read(true).open(path).unwrap()
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[test]
#[ignore = "requires an explicitly selected installed 7-Zip 24.09 DLL"]
fn installed_2409_sdk_stream_custody_and_verification() {
    let dll = PathBuf::from(
        std::env::var_os("GPUI_FILER_7Z_2409_DLL")
            .expect("set GPUI_FILER_7Z_2409_DLL to the resolved 24.09 7z.dll path"),
    );
    assert!(
        dll.is_absolute() && dll.is_file(),
        "the explicit 24.09 SDK fixture DLL must be installed"
    );
    let root = tempfile::tempdir().unwrap();
    let first_path = root.path().join("unicode-source.bin");
    let empty_path = root.path().join("empty-source.bin");
    let first_bytes = b"exact SDK source bytes";
    fs::write(&first_path, first_bytes).unwrap();
    fs::write(&empty_path, b"").unwrap();
    let first = open_source(&first_path);
    let empty = open_source(&empty_path);
    let candidate = root.path().join("candidate.7z");
    let mut parent_candidate = create_new_candidate(&candidate);
    let child_output = duplicate_inheritable(
        HANDLE(parent_candidate.as_raw_handle() as *mut _),
        GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let child_first = duplicate_inheritable(
        HANDLE(first.as_raw_handle() as *mut _),
        GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let child_empty = duplicate_inheritable(
        HANDLE(empty.as_raw_handle() as *mut _),
        GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let args = vec![
        "--exact".into(),
        "--ignored".into(),
        "archive_provider::tests::seven_zip_sdk_child_fixture".into(),
        "--nocapture".into(),
        "--".into(),
        "--sdk-child=success".into(),
        format!("--sdk-dll={}", dll.display()).into(),
        format!("--sdk-output={}", child_output.0.0 as usize).into(),
        format!(
            "--sdk-item=file|{}|{}|日本語.txt",
            child_first.0.0 as usize,
            first_bytes.len()
        )
        .into(),
        format!("--sdk-item=file|{}|0|empty.bin", child_empty.0.0 as usize).into(),
        "--sdk-item=dir||0|empty-dir".into(),
    ];
    let inherited = [child_output.0, child_first.0, child_empty.0];
    let outcome = crate::archive_provider::launch_sdk_test_child(
        &dll,
        &args,
        root.path(),
        Instant::now() + Duration::from_secs(90),
        &AtomicBool::new(false),
        &inherited,
    );
    assert_eq!(
        outcome,
        Some(0),
        "the contained 24.09 SDK child must finish cleanly"
    );
    drop((child_output, child_first, child_empty));

    let profile_candidate = root.path().join("unsupported-profile.7z");
    let parent_profile = create_new_candidate(&profile_candidate);
    let child_profile_output = duplicate_inheritable(
        HANDLE(parent_profile.as_raw_handle() as *mut _),
        GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let child_profile_source = duplicate_inheritable(
        HANDLE(first.as_raw_handle() as *mut _),
        GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let profile_args = vec![
        "--exact".into(),
        "--ignored".into(),
        "archive_provider::tests::seven_zip_sdk_child_fixture".into(),
        "--nocapture".into(),
        "--".into(),
        "--sdk-child=bad-profile".into(),
        format!("--sdk-dll={}", dll.display()).into(),
        format!("--sdk-output={}", child_profile_output.0.0 as usize).into(),
        format!(
            "--sdk-item=file|{}|{}|profile-check.txt",
            child_profile_source.0.0 as usize,
            first_bytes.len()
        )
        .into(),
    ];
    let profile_handles = [child_profile_output.0, child_profile_source.0];
    let profile_outcome = crate::archive_provider::launch_sdk_test_child(
        &dll,
        &profile_args,
        root.path(),
        Instant::now() + Duration::from_secs(90),
        &AtomicBool::new(false),
        &profile_handles,
    );
    assert_eq!(
        profile_outcome,
        Some(0),
        "installed 24.09 must reject an unsupported test profile before output mutation"
    );
    drop((child_profile_output, child_profile_source));
    assert_eq!(parent_profile.metadata().unwrap().len(), 0);

    let cancelled_candidate = root.path().join("cancelled-candidate.7z");
    let parent_cancelled = create_new_candidate(&cancelled_candidate);
    let child_cancelled_output = duplicate_inheritable(
        HANDLE(parent_cancelled.as_raw_handle() as *mut _),
        GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let child_cancelled_source = duplicate_inheritable(
        HANDLE(first.as_raw_handle() as *mut _),
        GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let cancel_args = vec![
        "--exact".into(),
        "--ignored".into(),
        "archive_provider::tests::seven_zip_sdk_child_fixture".into(),
        "--nocapture".into(),
        "--".into(),
        "--sdk-child=cancel".into(),
        format!("--sdk-dll={}", dll.display()).into(),
        format!("--sdk-output={}", child_cancelled_output.0.0 as usize).into(),
        format!(
            "--sdk-item=file|{}|{}|cancel.txt",
            child_cancelled_source.0.0 as usize,
            first_bytes.len()
        )
        .into(),
    ];
    let cancelled_handles = [child_cancelled_output.0, child_cancelled_source.0];
    assert_eq!(
        crate::archive_provider::launch_sdk_test_child(
            &dll,
            &cancel_args,
            root.path(),
            Instant::now() + Duration::from_secs(90),
            &AtomicBool::new(false),
            &cancelled_handles,
        ),
        Some(0),
        "SDK cancellation must return through the drained provider child",
    );
    drop((child_cancelled_output, child_cancelled_source));
    assert_eq!(parent_cancelled.metadata().unwrap().len(), 0);
    assert!(fs::remove_file(&cancelled_candidate).is_err());

    assert!(OpenOptions::new().write(true).open(&candidate).is_err());
    assert!(fs::rename(&candidate, root.path().join("renamed.7z")).is_err());
    assert!(fs::remove_file(&candidate).is_err());
    let preexisting = root.path().join("preexisting.7z");
    fs::write(&preexisting, b"sentinel").unwrap();
    let preexisting_wide = wide(preexisting.as_os_str());
    assert!(
        unsafe {
            CreateFileW(
                PCWSTR(preexisting_wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ,
                None,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .is_err()
    );
    assert_eq!(fs::read(&preexisting).unwrap(), b"sentinel");

    let stage = root.path().join("protected-stage.7z");
    let mut parent_stage = create_new_candidate(&stage);
    parent_candidate.seek(std::io::SeekFrom::Start(0)).unwrap();
    std::io::copy(&mut parent_candidate, &mut parent_stage).unwrap();
    parent_stage.flush().unwrap();
    let manifest = [
        ManifestEntry {
            name: "日本語.txt".into(),
            kind: EntryKind::File,
            length: first_bytes.len() as u64,
            sha256: hash(first_bytes),
        },
        ManifestEntry {
            name: "empty.bin".into(),
            kind: EntryKind::File,
            length: 0,
            sha256: hash(b""),
        },
        ManifestEntry {
            name: "empty-dir".into(),
            kind: EntryKind::Directory,
            length: 0,
            sha256: [0; 32],
        },
    ];
    parent_stage.seek(std::io::SeekFrom::Start(0)).unwrap();
    verify::verify(
        &mut parent_stage,
        &manifest,
        Instant::now() + Duration::from_secs(30),
        &AtomicBool::new(false),
    )
    .expect("SDK output must pass the independent stage verifier");
    assert!(parent_candidate.metadata().unwrap().is_file());
}

#[test]
#[ignore = "requires fresh application release executable and installed 7-Zip 24.09"]
fn actual_application_helper_preserves_parent_candidate_and_verifies_archive() {
    let app = PathBuf::from(
        std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_V6_TEST_EXECUTABLE to the fresh v6 release executable"),
    );
    let seven_zip = PathBuf::from(
        std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to the installed 7z.exe"),
    );
    assert!(app.is_absolute() && app.is_file());
    assert!(seven_zip.is_absolute() && seven_zip.is_file());

    let root = tempfile::tempdir().unwrap();
    let source_bytes = b"actual main binary helper source";
    let empty_bytes: &[u8] = b"";
    let source_path = root.path().join("source.bin");
    let empty_path = root.path().join("empty.bin");
    fs::write(&source_path, source_bytes).unwrap();
    fs::write(&empty_path, empty_bytes).unwrap();
    let source = open_source(&source_path);
    let empty_leading = open_source(&empty_path);
    let empty_dash = open_source(&empty_path);

    let owner = crate::archive_provider::ArchiveProviderOwner::default();
    let revision = owner.begin_discovery();
    let executable = crate::archive_provider::discover_from_path(
        Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
        root.path().to_path_buf(),
    )
    .expect("installed provider discovery must capture the 7z binding");
    assert_eq!(executable.path, seven_zip);
    let binding = executable
        .execution_hints
        .operation_binding
        .clone()
        .expect("discovery must bind the executable and loaded DLL");
    assert!(owner.commit_discovery(revision, Some(executable)));
    let provider = owner
        .lease_7z_creation(revision, &binding)
        .expect("the current discovered provider must admit 7z creation");
    let image = crate::archive_provider::application_image::lease_explicit_path_for_test(&app)
        .expect("explicit actual application image must be leased");

    let candidate_path = root.path().join("actual-helper.7z");
    let mut candidate = create_new_candidate(&candidate_path);
    let sentinel_path = root.path().join("sentinel.7z");
    fs::write(&sentinel_path, b"sentinel").unwrap();
    assert!(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sentinel_path)
            .is_err()
    );
    let entries = [
        crate::archive_provider::SevenZipHelperEntry {
            name: "日本語.txt".into(),
            directory: false,
            expected_length: source_bytes.len() as u64,
            source: Some(&source),
        },
        crate::archive_provider::SevenZipHelperEntry {
            name: "@leading.txt".into(),
            directory: false,
            expected_length: 0,
            source: Some(&empty_leading),
        },
        crate::archive_provider::SevenZipHelperEntry {
            name: "-leading.bin".into(),
            directory: false,
            expected_length: 0,
            source: Some(&empty_dash),
        },
        crate::archive_provider::SevenZipHelperEntry {
            name: "empty-dir".into(),
            directory: true,
            expected_length: 0,
            source: None,
        },
    ];
    let launch = crate::archive_provider::build_seven_zip_helper_launch(
        provider, image, &candidate, &entries,
    )
    .expect("parent builder must admit the exact handle bundle");
    assert_eq!(
        crate::archive_provider::run_seven_zip_helper_launch(
            launch,
            root.path(),
            Instant::now() + Duration::from_secs(90),
            &AtomicBool::new(false),
        ),
        crate::archive_provider::SevenZipHelperRunOutcome::Exited { code: 0 }
    );

    let stage_path = root.path().join("retained-stage.7z");
    let mut stage = create_new_candidate(&stage_path);
    candidate.seek(std::io::SeekFrom::Start(0)).unwrap();
    std::io::copy(&mut candidate, &mut stage).unwrap();
    stage.flush().unwrap();
    let manifest = [
        ManifestEntry {
            name: "日本語.txt".into(),
            kind: EntryKind::File,
            length: source_bytes.len() as u64,
            sha256: hash(source_bytes),
        },
        ManifestEntry {
            name: "@leading.txt".into(),
            kind: EntryKind::File,
            length: 0,
            sha256: hash(empty_bytes),
        },
        ManifestEntry {
            name: "-leading.bin".into(),
            kind: EntryKind::File,
            length: 0,
            sha256: hash(empty_bytes),
        },
        ManifestEntry {
            name: "empty-dir".into(),
            kind: EntryKind::Directory,
            length: 0,
            sha256: [0; 32],
        },
    ];
    stage.seek(std::io::SeekFrom::Start(0)).unwrap();
    verify::verify(
        &mut stage,
        &manifest,
        Instant::now() + Duration::from_secs(30),
        &AtomicBool::new(false),
    )
    .expect("actual executable helper output must pass the independent verifier");

    assert_eq!(fs::read(sentinel_path).unwrap(), b"sentinel");
}
