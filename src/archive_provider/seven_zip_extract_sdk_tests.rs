#![cfg(windows)]

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{
            BOOL, CloseHandle, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, GetHandleInformation,
            HANDLE, HANDLE_FLAG_INHERIT,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        },
        System::Threading::{GetCurrentProcess, SYNCHRONIZATION_SYNCHRONIZE},
    },
    core::PCWSTR,
};

use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};

use super::super::{extract_archive, inspect_archive};
use crate::archive_provider::seven_zip_extract_plan::Validated7zExtractPlan;

struct InheritedHandle(HANDLE);

impl Drop for InheritedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn duplicate_inheritable(file: &File, access: u32) -> InheritedHandle {
    let mut duplicate = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(file.as_raw_handle() as *mut _),
            GetCurrentProcess(),
            &mut duplicate,
            access,
            BOOL(1),
            DUPLICATE_HANDLE_OPTIONS(0),
        )
        .expect("duplicate narrow inheritable fixture handle");
    }
    InheritedHandle(duplicate)
}

fn spool_file(path: &Path) -> File {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0
                | GENERIC_WRITE.0
                | FILE_READ_ATTRIBUTES.0
                | SYNCHRONIZATION_SYNCHRONIZE.0,
            FILE_SHARE_READ,
            None,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .expect("create one empty spool file");
    unsafe { File::from_raw_handle(handle.0) }
}

fn required_path(name: &str) -> PathBuf {
    let value =
        std::env::var_os(name).unwrap_or_else(|| panic!("set {name} for ignored SDK fixture"));
    let path = PathBuf::from(value);
    assert!(
        path.is_absolute() && path.is_dir(),
        "{name} must name an existing directory"
    );
    path
}

fn child_args(mode: &str, dll: &Path, source: HANDLE, spool: HANDLE, length: u64) -> Vec<OsString> {
    vec![
        "--exact".into(),
        "--ignored".into(),
        "archive_provider::seven_zip_reader_sdk::seven_zip_extract_sdk::sdk_fixture_tests::decode_child".into(),
        "--nocapture".into(),
        "--".into(),
        format!("--decode-child={mode}").into(),
        format!("--decode-dll={}", dll.display()).into(),
        format!("--decode-source={}", source.0 as usize).into(),
        format!("--decode-spool={}", spool.0 as usize).into(),
        format!("--decode-length={length}").into(),
    ]
}

#[test]
#[ignore = "requires explicit installed 7-Zip 24.09 and per-volume fixture roots"]
fn installed_sdk_decodes_payload_into_parent_retained_spool() {
    let executable = std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
        .map(PathBuf::from)
        .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to installed 7z.exe");
    assert!(executable.is_absolute() && executable.is_file());
    let dll = executable.with_file_name("7z.dll");
    assert!(
        dll.is_file(),
        "the explicit provider's adjacent 7z.dll is required"
    );
    let fixture_root = required_path("GPUI_FILER_7ZIP_FIXTURE_ROOT");
    let source_root = required_path("GPUI_FILER_7ZIP_EXPECTED_SOURCE_ROOT");
    let output_root = required_path("GPUI_FILER_7ZIP_DECODE_OUTPUT_ROOT");
    let mixed_expected = [
        fs::read(source_root.join("root-a/nested/a.txt")).unwrap(),
        fs::read(source_root.join("root-b/-name.bin")).unwrap(),
        fs::read(source_root.join("日本語.txt")).unwrap(),
    ]
    .concat();
    assert_eq!(
        mixed_expected.len(),
        8068,
        "known source fixture shape changed"
    );
    for name in ["default-lzma2", "lzma", "deflate"] {
        eprintln!("SDK decode positive archive: {name}");
        assert_decodes_bytes(
            &fixture_root.join(format!("{name}.7z")),
            &dll,
            &output_root,
            &mixed_expected,
        );
    }
    for name in ["empty", "single-empty-file", "unix-regular"] {
        eprintln!("SDK decode zero-byte archive: {name}");
        assert_decodes_bytes(
            &fixture_root.join(format!("{name}.7z")),
            &dll,
            &output_root,
            b"",
        );
    }
}

fn assert_decodes_bytes(archive: &Path, dll: &Path, output_root: &Path, expected: &[u8]) {
    assert!(
        archive.is_file(),
        "fixture archive missing: {}",
        archive.display()
    );
    eprintln!("SDK positive fixture archive={}", archive.display());
    let source = File::open(archive).expect("open archive read-only");
    let length = source.metadata().unwrap().len();
    let source_child = duplicate_inheritable(
        &source,
        GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let spool_dir =
        tempfile::tempdir_in(output_root).expect("create spool directory on target volume");
    let spool = spool_file(&spool_dir.path().join("payload.spool"));
    let spool_child = duplicate_inheritable(
        &spool,
        GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let inherited = [source_child.0, spool_child.0];
    let args = child_args("decode", dll, source_child.0, spool_child.0, length);
    let outcome = crate::archive_provider::launch_sdk_test_child(
        dll,
        &args,
        archive.parent().unwrap(),
        Instant::now() + Duration::from_secs(120),
        &AtomicBool::new(false),
        &inherited,
    );
    assert_eq!(
        outcome,
        Some(0),
        "contained SDK child must drain cleanly for {}",
        archive.display()
    );
    drop((source_child, spool_child));
    let mut spool = spool;
    spool.seek(SeekFrom::Start(0)).unwrap();
    let mut actual = Vec::new();
    spool.read_to_end(&mut actual).unwrap();
    assert_eq!(
        actual,
        expected,
        "decoded bytes differ for {}",
        archive.display()
    );
}

#[test]
#[ignore = "requires explicit installed 7-Zip 24.09 and derived payload-corrupt fixture"]
fn installed_sdk_rejects_valid_plan_mismatches_corruption_nonempty_and_precancel() {
    let executable = PathBuf::from(
        std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE"),
    );
    assert!(executable.is_absolute() && executable.is_file());
    let dll = executable.with_file_name("7z.dll");
    assert!(dll.is_file());
    let fixture_root = required_path("GPUI_FILER_7ZIP_FIXTURE_ROOT");
    let output_root = required_path("GPUI_FILER_7ZIP_DECODE_OUTPUT_ROOT");
    let mixed = fixture_root.join("default-lzma2.7z");
    for field in ["name", "index", "size", "crc"] {
        eprintln!("SDK rejection plan mismatch: {field}");
        assert_rejected_without_mutation(
            &mixed,
            &dll,
            &output_root,
            &format!("mismatch-{field}"),
            b"",
        );
    }
    eprintln!("SDK rejection nonempty spool");
    assert_rejected_without_mutation(&mixed, &dll, &output_root, "nonempty", b"sentinel");
    eprintln!("SDK rejection precancel");
    assert_rejected_without_mutation(&mixed, &dll, &output_root, "precancel", b"");
    let corrupt = PathBuf::from(
        std::env::var_os("GPUI_FILER_7ZIP_DECODE_CORRUPT")
            .expect("set GPUI_FILER_7ZIP_DECODE_CORRUPT to derived payload-corrupt.7z"),
    );
    assert!(corrupt.is_absolute() && corrupt.is_file());
    eprintln!("SDK payload corruption after successful metadata inspection");
    let bytes = run_rejected_child(&corrupt, &dll, &output_root, "corrupt", b"");
    assert!(
        bytes.len() <= 8068,
        "corrupt decode must remain within planned spool bound"
    );

    let trailing = PathBuf::from(
        std::env::var_os("GPUI_FILER_7ZIP_DECODE_TRAILING")
            .expect("set GPUI_FILER_7ZIP_DECODE_TRAILING to packed-trailing.7z"),
    );
    assert!(trailing.is_absolute() && trailing.is_file());
    let source_root = required_path("GPUI_FILER_7ZIP_EXPECTED_SOURCE_ROOT");
    let expected = [
        fs::read(source_root.join("root-a/nested/a.txt")).unwrap(),
        fs::read(source_root.join("root-b/-name.bin")).unwrap(),
        fs::read(source_root.join("日本語.txt")).unwrap(),
    ]
    .concat();
    eprintln!("SDK trailing payload report after all planned writes");
    let bytes = run_rejected_child(&trailing, &dll, &output_root, "trailing", b"");
    assert_eq!(bytes.len(), 8068);
    assert_eq!(
        bytes, expected,
        "late data-after-end rejection preserves exact decoded bytes"
    );
}

fn assert_rejected_without_mutation(
    archive: &Path,
    dll: &Path,
    output_root: &Path,
    mode: &str,
    sentinel: &[u8],
) {
    let after = run_rejected_child(archive, dll, output_root, mode, sentinel);
    assert_eq!(
        after, sentinel,
        "rejected case {mode} must preserve parent bytes"
    );
}

fn run_rejected_child(
    archive: &Path,
    dll: &Path,
    output_root: &Path,
    mode: &str,
    sentinel: &[u8],
) -> Vec<u8> {
    eprintln!("SDK rejection mode={mode} archive={}", archive.display());
    let source = File::open(archive).expect("open fixture archive read-only");
    let length = source.metadata().unwrap().len();
    let source_child = duplicate_inheritable(
        &source,
        GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let spool_dir =
        tempfile::tempdir_in(output_root).expect("spool lives on selected output volume");
    let mut spool = spool_file(&spool_dir.path().join("payload.spool"));
    spool.write_all(sentinel).unwrap();
    spool.seek(SeekFrom::Start(0)).unwrap();
    let spool_child = duplicate_inheritable(
        &spool,
        GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
    );
    let inherited = [source_child.0, spool_child.0];
    let args = child_args(mode, dll, source_child.0, spool_child.0, length);
    assert_eq!(
        crate::archive_provider::launch_sdk_test_child(
            dll,
            &args,
            archive.parent().unwrap(),
            Instant::now() + Duration::from_secs(120),
            &AtomicBool::new(false),
            &inherited,
        ),
        Some(0),
        "contained SDK rejection case {mode} must drain and validate its expected error",
    );
    drop((source_child, spool_child));
    spool.seek(SeekFrom::Start(0)).unwrap();
    let mut after = Vec::new();
    spool.read_to_end(&mut after).unwrap();
    after
}

// Test-harness child: only the two explicit inherited handles carry archive/spool authority.
#[test]
#[ignore = "launched only by installed_sdk_decodes_payload_into_parent_retained_spool"]
fn decode_child() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--decode-child="))
        .expect("decode_child is only valid as an explicitly launched helper");
    assert!(matches!(
        mode,
        "decode"
            | "mismatch-name"
            | "mismatch-index"
            | "mismatch-size"
            | "mismatch-crc"
            | "nonempty"
            | "precancel"
            | "corrupt"
            | "trailing"
    ));
    let arg = |name: &str| {
        args.iter()
            .find_map(|item| item.strip_prefix(name))
            .unwrap_or_else(|| panic!("missing child argument {name}"))
    };
    let dll = PathBuf::from(arg("--decode-dll="));
    let source_raw = arg("--decode-source=").parse::<usize>().unwrap();
    let spool_raw = arg("--decode-spool=").parse::<usize>().unwrap();
    let length = arg("--decode-length=").parse::<u64>().unwrap();
    assert_ne!(source_raw, 0);
    assert_ne!(source_raw, usize::MAX);
    assert_ne!(spool_raw, 0);
    assert_ne!(spool_raw, usize::MAX);
    assert_ne!(source_raw, spool_raw, "inherited handles must be distinct");
    for raw in [source_raw, spool_raw] {
        let mut flags = 0;
        unsafe { GetHandleInformation(HANDLE(raw as *mut _), &mut flags) }
            .expect("inherited handle must be valid");
        assert_ne!(
            flags & HANDLE_FLAG_INHERIT.0,
            0,
            "raw handle must be inheritable"
        );
    }
    let source = unsafe { File::from_raw_handle(source_raw as _) };
    let spool = unsafe { File::from_raw_handle(spool_raw as _) };
    assert!(source.metadata().unwrap().is_file());
    assert_eq!(source.metadata().unwrap().len(), length);
    assert!(spool.metadata().unwrap().is_file());
    let initial_spool_length = if mode == "nonempty" { 8 } else { 0 };
    assert_eq!(spool.metadata().unwrap().len(), initial_spool_length);

    // Both calls use one continuously leased exact SDK module in this contained child.
    let _module_lease = super::super::Module::load(&dll)
        .expect("hold the exact provider DLL across inspect and extraction");
    // Inspecting first both proves the actual SDK metadata path and seeds the exact plan.
    let mut records = inspect_archive(&dll, source.try_clone().unwrap(), length)
        .expect("SDK metadata inspection must succeed before payload decode");
    let actual_plan =
        Validated7zExtractPlan::validate(records.clone()).expect("actual plan validates");
    match mode {
        "mismatch-name" => {
            let record = records
                .iter_mut()
                .find(|record| record.kind == crate::archive_manifest::NodeKind::File)
                .expect("mixed archive has a file");
            record.components.last_mut().unwrap().push_str("-different");
        }
        "mismatch-index" => {
            assert!(records.len() >= 2);
            records.swap(0, 1);
            let first = records[0].index;
            records[0].index = records[1].index;
            records[1].index = first;
        }
        "mismatch-size" => {
            let record = records
                .iter_mut()
                .find(|record| record.kind == crate::archive_manifest::NodeKind::File)
                .expect("mixed archive has a file");
            record.declared_size = Some(record.declared_size.unwrap() + 1);
        }
        "mismatch-crc" => {
            let record = records
                .iter_mut()
                .find(|record| record.kind == crate::archive_manifest::NodeKind::File)
                .expect("mixed archive has a file");
            record.crc32 = Some(!record.crc32.unwrap_or(0));
        }
        "decode" | "nonempty" | "precancel" | "corrupt" | "trailing" => {}
        other => panic!("unknown child mode {other}"),
    }
    let candidate_plan =
        Validated7zExtractPlan::validate(records).expect("candidate plan validates");
    if mode.starts_with("mismatch-") {
        assert_ne!(
            candidate_plan, actual_plan,
            "mismatch-{mode} must differ from actual metadata"
        );
    } else {
        assert_eq!(candidate_plan, actual_plan);
    }
    let plan = Arc::new(candidate_plan);
    let cancelled = Arc::new(AtomicBool::new(mode == "precancel"));
    let (result, report_observed) = if mode == "trailing" {
        super::extract_archive_observed(&dll, source, length, plan, spool, cancelled)
    } else {
        (
            extract_archive(&dll, source, length, plan, spool, cancelled),
            Arc::new(AtomicBool::new(false)),
        )
    };
    if mode == "decode" {
        result.expect("SDK payload decoding must succeed");
    } else if mode == "precancel" {
        assert_eq!(result, Err(super::E_ABORT));
    } else if mode == "trailing" {
        assert_eq!(result, Err(super::super::E_FAIL));
        assert!(
            report_observed.load(Ordering::Acquire),
            "the SDK reported archive failure only after all entries completed"
        );
    } else {
        assert!(result.is_err(), "SDK must reject child mode {mode}");
    }
}
