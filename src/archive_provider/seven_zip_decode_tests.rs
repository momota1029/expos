use super::*;

#[cfg(windows)]
use crate::{
    archive_manifest::NodeKind,
    archive_provider::{ArchiveProviderOwner, discover_from_path},
};

#[test]
#[ignore = "requires installed 7-Zip, release image, fixtures and NTFS/exFAT output root"]
fn installed_decode_transport_roundtrips_and_fails_closed() {
    use std::{
        io::{Read, Seek, SeekFrom, Write},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        sync::atomic::AtomicBool,
    };
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
        FILE_READ_DATA, FILE_SHARE_READ, FILE_WRITE_DATA,
    };

    let app = env_path("GPUI_FILER_V6_TEST_EXECUTABLE");
    let provider_path = env_path("GPUI_FILER_7ZIP_TEST_EXECUTABLE");
    let fixture_root = env_path("GPUI_FILER_7ZIP_FIXTURE_ROOT");
    let expected_root = env_path("GPUI_FILER_7ZIP_EXPECTED_SOURCE_ROOT");
    let output_root = env_path("GPUI_FILER_7ZIP_DECODE_OUTPUT_ROOT");
    let corrupt_path = env_path("GPUI_FILER_7ZIP_DECODE_CORRUPT");
    let trailing_path = env_path("GPUI_FILER_7ZIP_DECODE_TRAILING");
    let provider = discover_from_path(
        Some(provider_path.parent().unwrap().as_os_str().to_owned()),
        std::env::current_dir().unwrap(),
    )
    .expect("installed 7-Zip provider");
    let owner = ArchiveProviderOwner::default();
    let revision = owner.begin_discovery();
    assert!(owner.commit_discovery(revision, Some(provider)));
    let capability = owner.capture_7z_read_capability().unwrap();
    let lease = owner
        .lease_7z_extraction(
            &capability,
            super::super::application_image::lease_explicit_path_for_test(&app).unwrap(),
        )
        .expect("exact release image and provider admission");
    let cwd = std::env::current_dir().unwrap();
    let stop = AtomicBool::new(false);

    for archive_name in [
        "default-lzma2.7z",
        "lzma.7z",
        "deflate.7z",
        "empty.7z",
        "single-empty-file.7z",
        "unix-regular.7z",
    ] {
        let archive = fixture_root.join(archive_name);
        let original = std::fs::read(&archive).unwrap();
        let source = open_strict_source(&archive);
        let plan = owner
            .inspect_7z_archive(
                &lease,
                &source,
                &cwd,
                Instant::now() + Duration::from_secs(90),
                &stop,
            )
            .unwrap_or_else(|error| panic!("{archive_name} inspect: {error:?}"));
        let (spool_path, mut spool) = create_spool(&output_root, archive_name, 0);
        let result = run_parent(
            lease.clone(),
            &source,
            &mut spool,
            Arc::new(plan.clone()),
            &cwd,
            Instant::now() + Duration::from_secs(90),
            &stop,
        );
        let receipt = result.unwrap_or_else(|error| panic!("{archive_name} decode: {error:?}"));
        assert_eq!(
            receipt.final_total,
            plan.total_size(),
            "{archive_name} receipt"
        );
        spool.seek(SeekFrom::Start(0)).unwrap();
        let mut actual = Vec::new();
        spool.read_to_end(&mut actual).unwrap();
        assert_eq!(
            actual.len() as u64,
            plan.total_size(),
            "{archive_name} total"
        );
        for entry in plan.entries() {
            if entry.kind() != NodeKind::File {
                continue;
            }
            let range = entry.spool_range().unwrap();
            let start = usize::try_from(range.start).unwrap();
            let end = usize::try_from(range.end).unwrap();
            if start == end {
                assert!(actual[start..end].is_empty(), "empty file payload");
                continue;
            }
            let path = entry
                .components()
                .iter()
                .fold(expected_root.clone(), |root, part| root.join(part));
            let expected = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("expected source {}: {error}", path.display()));
            assert_eq!(
                &actual[start..end],
                expected.as_slice(),
                "{archive_name}: {:?}",
                entry.components()
            );
        }
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            original,
            "source unchanged"
        );
        eprintln!("decode fixture {archive_name}: ok ({}) bytes", actual.len());
        drop(spool);
        std::fs::remove_file(spool_path).unwrap();
    }

    // SDK の再列挙照合が payload 出力より先に plan 差分を拒否することを確認する。
    let archive = fixture_root.join("default-lzma2.7z");
    let source = open_strict_source(&archive);
    let records = indexed_records(
        owner
            .inspect_7z_archive(
                &lease,
                &source,
                &cwd,
                Instant::now() + Duration::from_secs(60),
                &stop,
            )
            .unwrap(),
    );
    for mismatch in ["name", "size", "crc"] {
        let mut changed = records.clone();
        let file = changed
            .iter_mut()
            .find(|record| record.kind == NodeKind::File)
            .unwrap();
        match mismatch {
            "name" => *file.components.last_mut().unwrap() = "not-the-original.txt".into(),
            "size" => file.declared_size = Some(file.declared_size.unwrap() + 1),
            _ => file.crc32 = Some(file.crc32.unwrap_or(0) ^ 1),
        }
        let wrong_plan = Arc::new(Validated7zExtractPlan::validate(changed).unwrap());
        let (path, mut spool) = create_spool(&output_root, "plan-mismatch", unique_id());
        assert!(
            run_parent(
                lease.clone(),
                &source,
                &mut spool,
                wrong_plan,
                &cwd,
                Instant::now() + Duration::from_secs(60),
                &stop
            )
            .is_err(),
            "{mismatch} mismatch must fail"
        );
        assert_eq!(spool.metadata().unwrap().len(), 0, "{mismatch} wrote bytes");
        eprintln!("decode adversary plan-{mismatch}-mismatch: rejected before writes");
        drop(spool);
        std::fs::remove_file(path).unwrap();
    }

    let good_plan = Arc::new(
        owner
            .inspect_7z_archive(
                &lease,
                &source,
                &cwd,
                Instant::now() + Duration::from_secs(60),
                &stop,
            )
            .unwrap(),
    );
    for (label, path) in [
        ("payload-corrupt", corrupt_path),
        ("packed-trailing", trailing_path),
    ] {
        let damaged_bytes = std::fs::read(&path).unwrap();
        let damaged = open_strict_source(&path);
        let damaged_plan = Arc::new(
            owner
                .inspect_7z_archive(
                    &lease,
                    &damaged,
                    &cwd,
                    Instant::now() + Duration::from_secs(60),
                    &stop,
                )
                .unwrap_or_else(|error| panic!("{label} inspect: {error:?}")),
        );
        let (spool_path, mut spool) = create_spool(&output_root, label, 2);
        let result = run_parent(
            lease.clone(),
            &damaged,
            &mut spool,
            damaged_plan.clone(),
            &cwd,
            Instant::now() + Duration::from_secs(60),
            &stop,
        );
        assert!(result.is_err(), "{label} must fail decode");
        let spool_len = spool.metadata().unwrap().len();
        assert!(
            spool_len <= damaged_plan.total_size(),
            "{label} exceeded the planned bound"
        );
        if label == "packed-trailing" {
            assert_eq!(
                spool_len,
                damaged_plan.total_size(),
                "late trailing-data failure follows full payload"
            );
            spool.seek(SeekFrom::Start(0)).unwrap();
            let mut actual = Vec::new();
            spool.read_to_end(&mut actual).unwrap();
            for entry in damaged_plan.entries() {
                if entry.kind() != NodeKind::File {
                    continue;
                }
                let range = entry.spool_range().unwrap();
                let start = usize::try_from(range.start).unwrap();
                let end = usize::try_from(range.end).unwrap();
                if start == end {
                    continue;
                }
                let expected_path = entry
                    .components()
                    .iter()
                    .fold(expected_root.clone(), |root, part| root.join(part));
                let expected = std::fs::read(&expected_path).unwrap();
                assert_eq!(
                    &actual[start..end],
                    expected.as_slice(),
                    "trailing data was rejected after exact payload"
                );
            }
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            damaged_bytes,
            "{label} source unchanged"
        );
        eprintln!("decode adversary {label}: rejected after {spool_len} bounded spool bytes");
        drop(spool);
        std::fs::remove_file(spool_path).unwrap();
    }

    // 継承 spool handle の権限を絞り、親が持つ SHARE_READ の保護を確認する。
    let source = open_strict_source(&archive);
    let plan = owner
        .inspect_7z_archive(
            &lease,
            &source,
            &cwd,
            Instant::now() + Duration::from_secs(60),
            &stop,
        )
        .unwrap();
    let (spool_path, mut spool) = create_spool(&output_root, "rights", 3);
    let launch = build_launch(lease.clone(), &source, &mut spool, Arc::new(plan)).unwrap();
    let access = query_granted_access(super::super::seven_zip_reader::raw_handle(
        &launch.transport.spool_child,
    ));
    assert_ne!(
        access & FILE_WRITE_DATA.0,
        0,
        "child handle retains WRITE_DATA"
    );
    assert_eq!(access & DELETE.0, 0, "narrow child handle lacks DELETE");
    assert_eq!(
        access & FILE_READ_DATA.0,
        0,
        "narrow child handle lacks READ_DATA"
    );
    assert!(OpenOptions::new().write(true).open(&spool_path).is_err());
    assert!(
        OpenOptions::new()
            .access_mode(DELETE.0)
            .open(&spool_path)
            .is_err()
    );
    assert!(OpenOptions::new().write(true).open(&archive).is_err());
    assert!(
        OpenOptions::new()
            .access_mode(DELETE.0)
            .open(&archive)
            .is_err()
    );
    eprintln!("decode custody: child rights narrow; independent write/delete opens denied");
    drop(launch);
    drop(spool);
    std::fs::remove_file(spool_path).unwrap();

    // 不適合 spool は decode helper に渡す前に拒否されることを確認する。
    let (sentinel_path, mut sentinel) = create_spool(&output_root, "sentinel", 4);
    sentinel.write_all(b"keep").unwrap();
    assert!(build_launch(lease.clone(), &source, &mut sentinel, good_plan.clone()).is_none());
    assert_eq!(std::fs::read(&sentinel_path).unwrap(), b"keep");
    eprintln!("decode adversary nonempty-spool: unchanged sentinel");
    drop(sentinel);
    std::fs::remove_file(sentinel_path).unwrap();

    let (alias_path, alias_source) = create_spool(&output_root, "alias-empty", unique_id());
    let mut source_alias = alias_source.try_clone().unwrap();
    let source_raw = windows::Win32::Foundation::HANDLE(alias_source.as_raw_handle().cast());
    let spool_raw = windows::Win32::Foundation::HANDLE(source_alias.as_raw_handle().cast());
    assert_ne!(
        source_raw.0, spool_raw.0,
        "file aliases have distinct handle values"
    );
    assert_eq!(
        super::super::seven_zip_reader::source_facts(source_raw),
        super::super::seven_zip_reader::source_facts(spool_raw),
        "file aliases share identity"
    );
    assert!(
        build_launch(
            lease.clone(),
            &alias_source,
            &mut source_alias,
            good_plan.clone()
        )
        .is_none()
    );
    drop(alias_source);
    drop(source_alias);
    std::fs::remove_file(alias_path).unwrap();
    eprintln!("decode adversary same-file alias: rejected");

    let (distinct_source_path, distinct_source) =
        create_spool(&output_root, "distinct-empty-source", unique_id());
    let (distinct_spool_path, mut distinct_spool) =
        create_spool(&output_root, "distinct-empty-spool", unique_id());
    let empty_plan = Arc::new(Validated7zExtractPlan::validate(Vec::new()).unwrap());
    let accepted = build_launch(
        lease.clone(),
        &distinct_source,
        &mut distinct_spool,
        empty_plan,
    )
    .expect("distinct empty ordinary files satisfy identity admission");
    drop(accepted);
    eprintln!("decode control distinct-empty-files: admitted");
    drop(distinct_source);
    drop(distinct_spool);
    std::fs::remove_file(distinct_source_path).unwrap();
    std::fs::remove_file(distinct_spool_path).unwrap();

    let (wrong_path, writable) = create_spool(&output_root, "wrong-rights", 5);
    let mut read_only = File::open(&wrong_path).unwrap();
    assert!(build_launch(lease.clone(), &source, &mut read_only, good_plan.clone()).is_none());
    eprintln!("decode adversary wrong-rights: rejected");
    drop(read_only);
    drop(writable);
    std::fs::remove_file(wrong_path).unwrap();

    let directory_path = output_root.join("decode-directory-kind");
    std::fs::create_dir(&directory_path).unwrap();
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(&directory_path)
        .unwrap();
    assert!(build_launch(lease.clone(), &source, &mut directory, good_plan.clone()).is_none());
    eprintln!("decode adversary wrong-kind: rejected");
    drop(directory);
    std::fs::remove_dir(directory_path).unwrap();

    for (label, flags) in [
        ("overlapped", FILE_FLAG_OVERLAPPED.0),
        ("unbuffered", FILE_FLAG_NO_BUFFERING.0),
    ] {
        let path = output_root.join(format!("decode-{label}-{}", unique_id()));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(flags)
            .open(&path)
            .unwrap();
        let result = run_parent(
            lease.clone(),
            &source,
            &mut file,
            good_plan.clone(),
            &cwd,
            Instant::now() + Duration::from_secs(30),
            &stop,
        );
        assert!(result.is_err(), "{label} spool must be rejected");
        assert_eq!(file.metadata().unwrap().len(), 0, "{label} spool unchanged");
        eprintln!("decode adversary {label}: rejected before writes");
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    let cancelled = AtomicBool::new(true);
    let (path, mut spool) = create_spool(&output_root, "precancel", 6);
    assert!(
        run_parent(
            lease.clone(),
            &source,
            &mut spool,
            good_plan.clone(),
            &cwd,
            Instant::now() + Duration::from_secs(30),
            &cancelled
        )
        .is_err()
    );
    assert_eq!(spool.metadata().unwrap().len(), 0);
    eprintln!("decode gate precancel: rejected without launch");
    drop(spool);
    std::fs::remove_file(path).unwrap();

    let (path, mut spool) = create_spool(&output_root, "deadline", 7);
    assert!(
        run_parent(
            lease,
            &source,
            &mut spool,
            good_plan,
            &cwd,
            Instant::now() - Duration::from_secs(1),
            &stop
        )
        .is_err()
    );
    assert_eq!(spool.metadata().unwrap().len(), 0);
    eprintln!("decode gate expired-deadline: rejected without launch");
    drop(spool);
    std::fs::remove_file(path).unwrap();
}

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

fn env_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set {name}"))
}

fn unique_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed) ^ std::process::id() as u64
}

fn create_spool(root: &Path, label: &str, ordinal: u64) -> (PathBuf, File) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ, FILE_WRITE_DATA, SYNCHRONIZE,
    };
    let path = root.join(format!("decode-{label}-{}-{ordinal}.spool", unique_id()));
    let access =
        FILE_READ_DATA.0 | FILE_WRITE_DATA.0 | DELETE.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(access)
        .share_mode(FILE_SHARE_READ.0)
        .open(&path)
        .unwrap_or_else(|error| panic!("create spool {}: {error}", path.display()));
    (path, file)
}

fn open_strict_source(path: &Path) -> File {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .unwrap_or_else(|error| panic!("open strict source {}: {error}", path.display()))
}

fn indexed_records(
    plan: Validated7zExtractPlan,
) -> Vec<super::super::seven_zip_extract_plan::IndexedRecord> {
    plan.entries()
        .iter()
        .map(
            |entry| super::super::seven_zip_extract_plan::IndexedRecord {
                index: entry.index(),
                components: entry.components().to_vec(),
                kind: entry.kind(),
                declared_size: entry.declared_size(),
                crc32: entry.crc32(),
            },
        )
        .collect()
}

#[repr(C)]
struct ObjectBasicInformation {
    attributes: u32,
    granted_access: u32,
    handle_count: u32,
    pointer_count: u32,
    paged_pool_usage: u32,
    nonpaged_pool_usage: u32,
    reserved: [u32; 3],
    name_information_length: u32,
    type_information_length: u32,
    security_descriptor_size: u32,
    creation_time: i64,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: windows::Win32::Foundation::HANDLE,
        information_class: u32,
        information: *mut std::ffi::c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

fn query_granted_access(handle: windows::Win32::Foundation::HANDLE) -> u32 {
    // カーネルが予約フィールドを省略しても未初期化値を読まない。
    let mut info: ObjectBasicInformation = unsafe { std::mem::zeroed() };
    let mut returned = 0;
    let status = unsafe {
        NtQueryObject(
            handle,
            0,
            (&mut info as *mut ObjectBasicInformation).cast(),
            std::mem::size_of::<ObjectBasicInformation>() as u32,
            &mut returned,
        )
    };
    assert_eq!(status, 0, "NtQueryObject(ObjectBasicInformation)");
    assert_eq!(
        returned as usize,
        std::mem::size_of::<ObjectBasicInformation>()
    );
    info.granted_access
}
