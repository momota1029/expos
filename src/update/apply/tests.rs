use super::*;
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;

#[cfg(unix)]
use crate::update::{
    Channel,
    source::{DirSource, check},
};

fn version(text: &str) -> VersionDir {
    VersionDir::from_version(text).unwrap()
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        writer
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn verifies_sha256_with_known_digest_and_rejects_changes() {
    let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    verify_sha256(&b"abc"[..], digest).unwrap();
    assert!(verify_sha256(&b"abd"[..], digest).is_err());
}

#[test]
fn rejects_unsafe_zip_paths_and_aliases() {
    for name in [
        "/absolute",
        "../outside",
        "a/../outside",
        "C:/root",
        "\\\\server\\file",
        "expos.exe:stream",
        "launcher/NUL.txt",
        "COM1 .txt",
        "LONGFI~1",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let bytes = zip(&[
            ("expos.exe", b"app"),
            (LAUNCHER, b"launcher"),
            ("VERSION.txt", b"1.2.3"),
            (name, b"x"),
        ]);
        assert!(
            extract(Cursor::new(bytes), dir.path(), &version("1.2.3")).is_err(),
            "{name}"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "{name}");
    }

    let dir = tempfile::tempdir().unwrap();
    let bytes = zip(&[("expos.exe", b"app"), ("Expos.exe", b"alias")]);
    assert!(extract(Cursor::new(bytes), dir.path(), &version("1.2.3")).is_err());

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_symlink("shortcut", "../outside", SimpleFileOptions::default())
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    assert!(extract(Cursor::new(bytes), dir.path(), &version("1.2.3")).is_err());
}

#[test]
fn rejects_windows_reparse_entry_attributes() {
    let mut bytes = zip(&[
        ("expos.exe", b"app"),
        (LAUNCHER, b"launcher"),
        ("VERSION.txt", b"1.2.3"),
    ]);
    let central = bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .unwrap();
    bytes[central + 39] |= 0x04;
    let dir = tempfile::tempdir().unwrap();
    assert!(extract(Cursor::new(bytes), dir.path(), &version("1.2.3")).is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn enforces_zip_limits_and_payload_contract() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = zip(&[
        ("expos.exe", b"app"),
        (LAUNCHER, b"launcher"),
        ("VERSION.txt", b"1.2.3"),
    ]);
    assert!(
        extract_with_limits(Cursor::new(&bytes), dir.path(), &version("1.2.3"), 2, 100).is_err()
    );
    assert!(
        extract_with_limits(Cursor::new(&bytes), dir.path(), &version("1.2.3"), 3, 10).is_err()
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(extract(Cursor::new(&bytes), dir.path(), &version("1.2.4")).is_err());

    let dir = tempfile::tempdir().unwrap();
    let bytes = zip(&[("expos.exe", b"app"), ("VERSION.txt", b"1.2.3")]);
    assert!(extract(Cursor::new(bytes), dir.path(), &version("1.2.3")).is_err());
}

#[cfg(unix)]
#[test]
fn dir_source_applies_reuses_and_checks_stale_or_corrupt_updates() {
    let root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let old = version("1.0.0");
    let new = version("1.2.3");
    fs::create_dir(old.path(root.path())).unwrap();
    fs::write(old.executable(root.path()), "old app").unwrap();
    lock_install_root(root.path())
        .unwrap()
        .activate(&old)
        .unwrap();

    let zip_name = "expos-v1.2.3-windows-x86_64.zip";
    let bytes = zip(&[
        ("expos.exe", b"new app"),
        (LAUNCHER, b"launcher"),
        ("VERSION.txt", b"1.2.3\n"),
    ]);
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let checksums = format!("{digest}  {zip_name}\n");
    fs::write(repo.path().join(zip_name), &bytes).unwrap();
    fs::write(repo.path().join("SHA256SUMS"), &checksums).unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "channel": "stable", "version": "1.2.3", "zip": zip_name, "sha256": digest,
    }))
    .unwrap();
    fs::write(repo.path().join("channel.json"), &manifest).unwrap();
    let asset = |name: &str, size: usize| {
        serde_json::json!({
            "name": name, "browser_download_url": format!("https://fixture.invalid/{name}"), "size": size,
        })
    };
    let releases = serde_json::json!([{"tag_name": "v1.2.3", "draft": false, "prerelease": false, "assets": [
        asset(zip_name, bytes.len()), asset("expos-v1.2.3-source-vendored.zip", 1),
        asset("SHA256SUMS", checksums.len()), asset("channel.json", manifest.len())
    ]}]);
    fs::write(repo.path().join("releases.json"), releases.to_string()).unwrap();

    let source = DirSource(repo.path().into());
    let offer = check(&source, "1.0.0", Channel::Stable, false)
        .unwrap()
        .unwrap();
    assert_eq!(offer.version, new);
    assert!(apply(root.path(), &source, &offer, None).is_err());
    fs::write(repo.path().join(zip_name), vec![0; bytes.len()]).unwrap();
    assert!(apply(root.path(), &source, &offer, Some(&old)).is_err());
    assert_eq!(read_current(root.path()).unwrap(), Some(old.clone()));
    assert!(!new.path(root.path()).exists());

    fs::write(repo.path().join(zip_name), &bytes).unwrap();
    apply(root.path(), &source, &offer, Some(&old)).unwrap();
    assert_eq!(read_current(root.path()).unwrap(), Some(new.clone()));
    assert_eq!(fs::read(new.executable(root.path())).unwrap(), b"new app");
    apply(root.path(), &source, &offer, Some(&new)).unwrap();

    let blocked_root = tempfile::tempdir().unwrap();
    fs::create_dir(blocked_root.path().join("current")).unwrap();
    assert!(apply(blocked_root.path(), &source, &offer, None).is_err());
    assert!(!new.path(blocked_root.path()).exists());
    assert!(blocked_root.path().join("current").is_dir());
    assert_eq!(
        fs::read_dir(root.path().join(".staging")).unwrap().count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn never_extracts_into_a_reparse_target() {
    let staging = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(other.path(), staging.path().join("launcher")).unwrap();
    let bytes = zip(&[
        ("expos.exe", b"app"),
        (LAUNCHER, b"launcher"),
        ("VERSION.txt", b"1.2.3"),
    ]);
    assert!(extract(Cursor::new(bytes), staging.path(), &version("1.2.3")).is_err());
    assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn cleanup_rolls_back_only_an_expired_unactivated_publication() {
    use std::fs::FileTimes;

    let root = tempfile::tempdir().unwrap();
    let old = version("1.0.0");
    let pending = version("1.2.3");
    let activated = version("1.3.0");
    for version in [&old, &pending, &activated] {
        fs::create_dir(version.path(root.path())).unwrap();
        fs::write(version.executable(root.path()), "payload").unwrap();
    }
    let guard = lock_install_root(root.path()).unwrap();
    guard.activate(&activated).unwrap();
    for version in [&pending, &activated] {
        let name = staging_name().unwrap();
        let stage = root.path().join(".staging").join(&name);
        fs::create_dir_all(&stage).unwrap();
        fs::write(
            stage.join(PUBLICATION),
            format!("{}\n{}\n", version.name(), activated.name()),
        )
        .unwrap();
        fs::write(version.path(root.path()).join(PENDING_MARKER), &name).unwrap();
        File::open(&stage)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - STAGING_TTL - Duration::from_secs(60)),
            )
            .unwrap();
    }
    cleanup_staging(root.path(), &guard);
    assert!(!pending.path(root.path()).exists());
    assert!(!activated.path(root.path()).join(PENDING_MARKER).exists());
    assert!(activated.executable(root.path()).exists());
    assert_eq!(read_current(root.path()).unwrap(), Some(activated));
    assert_eq!(
        fs::read_dir(root.path().join(".staging")).unwrap().count(),
        0
    );
}
