use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use install_layout::{InstallLock, VersionDir, lock_install_root, read_current};
use sha2::{Digest, Sha256};

use super::selection::checksum_for;
use super::source::{MAX_METADATA_BYTES, MAX_ZIP_BYTES, ReleaseSource, read_limited};
use super::{Offer, ordinary_file, ordinary_metadata};
use crate::zip_archive::{self, EntryKind};

const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const STAGING_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const LAUNCHER: &str = "launcher/expos-launcher.exe";
const PUBLICATION: &str = "publication";
const PENDING_MARKER: &str = ".expos-update-pending";

pub(crate) fn apply(
    root: &Path,
    source: &dyn ReleaseSource,
    offer: &Offer,
    expected_current: Option<&VersionDir>,
) -> io::Result<()> {
    if offer.tag != offer.version.name() {
        return Err(invalid("update tag and version do not match"));
    }
    let guard = lock_install_root(root)?;
    let current = read_current(root)?;
    if current.as_ref() != expected_current && current.as_ref() != Some(&offer.version) {
        return Err(io::Error::other(
            "the active installation changed; check again",
        ));
    }

    let staging_root = root.join(".staging");
    ensure_directory(&staging_root)?;
    cleanup_staging(root, &guard);
    let staging = tempfile::Builder::new()
        .prefix(&staging_name()?)
        .rand_bytes(0)
        .tempdir_in(staging_root)?;
    let sums = staging.path().join("SHA256SUMS");
    source.fetch_asset(&offer.sums_url, &sums, MAX_METADATA_BYTES)?;
    if checksum_for(&read_limited(&sums, MAX_METADATA_BYTES)?, &offer.zip_name).as_deref()
        != Some(&offer.sha256)
    {
        return Err(invalid("release checksums changed"));
    }
    if offer.size == 0 || offer.size > MAX_ZIP_BYTES {
        return Err(invalid("invalid update size"));
    }
    let archive = staging.path().join("update.zip");
    source.fetch_asset(&offer.zip_url, &archive, offer.size)?;
    let mut archive = File::open(&archive)?;
    if archive.metadata()?.len() != offer.size {
        return Err(invalid("downloaded ZIP size does not match the release"));
    }
    verify_sha256(&mut archive, &offer.sha256)?;
    archive.rewind()?;
    let payload = staging.path().join("payload");
    fs::create_dir(&payload)?;
    extract(archive, &payload, &offer.version)?;

    let dest = offer.version.path(root);
    let publish = ordinary_metadata(&dest)?.is_none();
    if publish {
        let name = staging
            .path()
            .file_name()
            .expect("staging has a name")
            .to_string_lossy();
        let previous = current.as_ref().map(VersionDir::name).unwrap_or_default();
        write_new(&payload.join(PENDING_MARKER), name.as_bytes())?;
        write_new(
            &staging.path().join(PUBLICATION),
            format!("{}\n{previous}\n", offer.tag).as_bytes(),
        )?;
        fs::rename(&payload, &dest)?;
    } else {
        // 部分インストールや同じタグの別ビルドを黙って上書きしない。
        validate_payload(&dest, &offer.version)?;
        match_payload(&payload, &dest, true)?;
    }
    if let Err(error) = guard.activate(&offer.version) {
        if publish && let Err(rollback) = fs::rename(&dest, &payload) {
            let recovery = staging.keep();
            return Err(io::Error::other(format!(
                "activation failed: {error}; could not roll back payload: {rollback}; recovery: {}",
                recovery.display(),
            )));
        }
        return Err(error);
    }
    let _ = fs::remove_file(dest.join(PENDING_MARKER));

    // current の commit 後にだけランチャーを更新する。失敗しても旧ランチャーで起動できる。
    #[cfg(windows)]
    refresh_launcher(root, &dest, staging.path());

    if let Err(error) = guard.prune(3) {
        eprintln!("[expos][update] could not prune old versions: {error}");
    }
    Ok(())
}

fn staging_name() -> io::Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    Ok(format!("update-{:032x}", u128::from_be_bytes(bytes)))
}

fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = File::options().write(true).create_new(true).open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

fn match_payload(staged: &Path, installed: &Path, allow_marker: bool) -> io::Result<()> {
    require_directory(installed)?;
    let staged_entries = fs::read_dir(staged)?.collect::<io::Result<Vec<_>>>()?;
    let mut count = 0;
    for entry in fs::read_dir(installed)? {
        let entry = entry?;
        if allow_marker && entry.file_name() == PENDING_MARKER {
            if !ordinary_file(&entry.path())? {
                return Err(invalid("invalid update publication marker"));
            }
        } else {
            count += 1;
        }
    }
    if count != staged_entries.len() {
        return Err(invalid(
            "existing version differs from the verified package",
        ));
    }
    for entry in staged_entries {
        let path = installed.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            match_payload(&entry.path(), &path, false)?;
        } else if !ordinary_file(&path)? || !same_file(&entry.path(), &path)? {
            return Err(invalid(
                "existing version differs from the verified package",
            ));
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    let mut a = File::open(a)?;
    let mut b = File::open(b)?;
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let mut a_hash = Sha256::new();
    let mut b_hash = Sha256::new();
    io::copy(&mut a, &mut a_hash)?;
    io::copy(&mut b, &mut b_hash)?;
    Ok(a_hash.finalize() == b_hash.finalize())
}

fn verify_sha256(input: impl Read, expected: &str) -> io::Result<()> {
    let mut input = input;
    let mut digest = Sha256::new();
    io::copy(&mut input, &mut digest)?;
    if format!("{:x}", digest.finalize()) != expected {
        return Err(invalid("downloaded ZIP SHA-256 does not match the release"));
    }
    Ok(())
}

fn extract(zip: impl Read + Seek, dest: &Path, version: &VersionDir) -> io::Result<()> {
    extract_with_limits(zip, dest, version, MAX_ENTRIES, MAX_UNPACKED_BYTES)
}

fn extract_with_limits(
    zip: impl Read + Seek,
    dest: &Path,
    version: &VersionDir,
    max_entries: usize,
    mut remaining: u64,
) -> io::Result<()> {
    require_directory(dest)?;
    let result = zip_archive::with_validated_archive_limit(
        zip,
        &AtomicBool::new(false),
        max_entries,
        |archive, nodes| {
            for node in nodes {
                if node.components[0].eq_ignore_ascii_case(PENDING_MARKER)
                    || node.components.len() > 32
                    || node.components.iter().any(|part| {
                        // 公開物の名前は ASCII。短い別名とデバイス名の別表記も拒否する。
                        !part.is_ascii()
                            || part.contains('~')
                            || part.len() > 255
                            || part.split_once('.').is_some_and(|(stem, _)| {
                                !stem.is_empty()
                                    && !crate::windows_name::is_single_name(stem.trim_end())
                            })
                    })
                {
                    return Err(zip_archive::ZipReadError("unsafe update ZIP path".into()));
                }
                let size = node.declared_uncompressed_size.unwrap_or(0);
                remaining = remaining.checked_sub(size).ok_or_else(|| {
                    zip_archive::ZipReadError("uncompressed ZIP exceeds its size limit".into())
                })?;
            }
            for node in nodes {
                let path = dest.join(node.components.iter().collect::<std::path::PathBuf>());
                if node.kind == EntryKind::Directory {
                    ensure_directory(&path)?;
                    continue;
                }
                let index = node.entry_index.expect("validated file has a ZIP entry");
                let size = node
                    .declared_uncompressed_size
                    .expect("validated file has a size");
                let mut entry = archive
                    .by_index(index)
                    .map_err(|error| zip_archive::ZipReadError(error.to_string()))?;
                // アーカイブの属性は復元せず、通常ファイルだけを新規作成する。
                let mut out = File::options().write(true).create_new(true).open(&path)?;
                if io::copy(&mut entry.by_ref().take(size + 1), &mut out)? != size {
                    return Err(zip_archive::ZipReadError(
                        "ZIP entry size does not match its header".into(),
                    ));
                }
                out.sync_all()?;
            }
            Ok(())
        },
    );
    result.map_err(|error| invalid(&error.to_string()))?;
    validate_payload(dest, version)
}

fn validate_payload(payload: &Path, version: &VersionDir) -> io::Result<()> {
    require_directory(payload)?;
    require_directory(&payload.join("launcher"))?;
    for name in ["expos.exe", LAUNCHER, "VERSION.txt"] {
        let path = payload.join(name);
        if !ordinary_file(&path)? || fs::metadata(path)?.len() == 0 {
            return Err(invalid("release payload is incomplete"));
        }
    }
    let expected = version.version().to_string();
    let bytes = read_limited(&payload.join("VERSION.txt"), 256)?;
    let bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(&bytes);
    if bytes != expected.as_bytes() {
        return Err(invalid("VERSION.txt does not match the offered version"));
    }
    Ok(())
}

fn require_directory(path: &Path) -> io::Result<()> {
    if ordinary_metadata(path)?.is_some_and(|meta| meta.is_dir()) {
        Ok(())
    } else {
        Err(invalid("expected an ordinary update directory"))
    }
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => require_directory(path),
        Err(error) => Err(error),
    }
}

pub(super) fn cleanup_staging(root: &Path, guard: &InstallLock) {
    let path = root.join(".staging");
    if require_directory(&path).is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let guid = name.to_str().and_then(|name| name.strip_prefix("update-"));
        if !guid.is_some_and(|guid| guid.len() == 32 && guid.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            continue;
        }
        let path = entry.path();
        let old = ordinary_metadata(&path).ok().flatten().is_some_and(|meta| {
            meta.is_dir()
                && meta
                    .modified()
                    .ok()
                    .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                    .is_some_and(|age| age >= STAGING_TTL)
        });
        if old {
            match recover_publication(root, &path, guard) {
                Ok(()) => {
                    let _ = fs::remove_dir_all(path);
                }
                Err(error) => eprintln!("[expos][update] staging cleanup deferred: {error}"),
            }
        }
    }
}

fn recover_publication(root: &Path, stage: &Path, guard: &InstallLock) -> io::Result<()> {
    let journal = stage.join(PUBLICATION);
    if !ordinary_file(&journal)? {
        return Ok(());
    }
    let bytes = read_limited(&journal, 1024)?;
    let mut lines = std::str::from_utf8(&bytes)
        .map_err(io::Error::other)?
        .lines();
    let version = lines
        .next()
        .and_then(VersionDir::parse)
        .ok_or_else(|| invalid("invalid pending update version"))?;
    let previous = match lines.next() {
        Some("") => None,
        Some(name) => Some(
            VersionDir::parse(name).ok_or_else(|| invalid("invalid previous update version"))?,
        ),
        None => return Err(invalid("missing previous update version")),
    };
    if lines.next().is_some() {
        return Err(invalid("unexpected pending update data"));
    }
    let dest = version.path(root);
    if !ordinary_metadata(&dest)?.is_some_and(|meta| meta.is_dir()) {
        return Ok(());
    }
    let marker = dest.join(PENDING_MARKER);
    let nonce = stage
        .file_name()
        .expect("staging has a name")
        .to_string_lossy();
    if !ordinary_file(&marker)? || read_limited(&marker, 128)? != nonce.as_bytes() {
        return Ok(());
    }
    let current = read_current(root)?;
    // 別のインストーラーが current を進めていたら、過去に有効だった可能性があるため版を残す。
    if current == previous && current.as_ref() != Some(&version) {
        if guard.is_version_in_use(&version)? {
            return Err(io::Error::other("pending update version is in use"));
        }
        fs::remove_dir_all(dest)
    } else {
        fs::remove_file(marker)
    }
}

#[cfg(windows)]
fn refresh_launcher(root: &Path, version: &Path, staging: &Path) {
    let launcher = root.join("expos.exe");
    let replacement = version.join(LAUNCHER);
    let needs_replacement = match ordinary_metadata(&launcher) {
        Ok(None) => true,
        Ok(Some(meta)) if meta.is_file() => {
            same_file(&launcher, &replacement).is_ok_and(|same| !same)
        }
        _ => false,
    };
    if !needs_replacement {
        return;
    }
    let copy = staging.join("launcher.exe");
    let result = (|| -> io::Result<()> {
        fs::copy(&replacement, &copy)?;
        File::options().write(true).open(&copy)?.sync_all()?;
        for attempt in 0..3 {
            match fs::rename(&copy, &launcher) {
                Ok(()) => return Ok(()),
                Err(error) if attempt == 2 => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(150)),
            }
        }
        unreachable!()
    })();
    if let Err(error) = result {
        eprintln!("[expos][update] could not refresh the launcher: {error}");
    }
}

fn invalid(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

#[cfg(test)]
mod tests;
