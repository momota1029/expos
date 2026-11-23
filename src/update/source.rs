use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::selection::{Release, ReleaseMetadata};
use super::{Channel, Offer, UTF8_BOM, selection, without_utf8_bom};
use install_layout::VersionDir;
use serde::de::DeserializeOwned;

pub(super) const MAX_ZIP_BYTES: u64 = 200 * 1024 * 1024;
pub(super) const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const RELEASES_JSON: &str = "releases.json";
const CHANNEL_JSON: &str = "channel.json";

#[derive(Debug)]
pub(super) struct UpdateInformationError;

impl fmt::Display for UpdateInformationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Couldn't read the update information")
    }
}

impl std::error::Error for UpdateInformationError {}

fn unreadable_metadata(
    document: &str,
    context: impl fmt::Display,
    error: impl fmt::Display,
) -> io::Error {
    eprintln!("[expos][update] {document}: {context}: {error}");
    io::Error::other(UpdateInformationError)
}

fn parse_metadata_json<T: DeserializeOwned>(document: &str, bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(without_utf8_bom(bytes)).map_err(|error| {
        let prefix = &bytes[..bytes.len().min(80)];
        let has_bom = prefix.starts_with(UTF8_BOM);
        let kind = match without_utf8_bom(prefix).trim_ascii_start() {
            [] if prefix.len() == bytes.len() => "empty",
            [] => "whitespace",
            [b'<', ..] => "HTML/markup",
            [b'{' | b'[', ..] => "JSON-like",
            _ => "other",
        };
        unreadable_metadata(
            document,
            format_args!(
                "invalid JSON, first {} bytes: {kind}, UTF-8 BOM={has_bom}",
                prefix.len()
            ),
            error,
        )
    })
}

pub(crate) trait ReleaseSource {
    fn list_releases(&self) -> io::Result<Vec<Release>>;
    fn fetch_asset(&self, url: &str, dest: &Path, max_bytes: u64) -> io::Result<()>;
}

pub(crate) struct DirSource(pub PathBuf);

impl ReleaseSource for DirSource {
    fn list_releases(&self) -> io::Result<Vec<Release>> {
        let bytes = read_limited(&self.0.join(RELEASES_JSON), MAX_METADATA_BYTES)
            .map_err(|error| unreadable_metadata(RELEASES_JSON, "read failed", error))?;
        parse_metadata_json(RELEASES_JSON, &bytes)
    }

    fn fetch_asset(&self, url: &str, dest: &Path, max_bytes: u64) -> io::Result<()> {
        let name = url.rsplit('/').next().unwrap_or_default();
        if !crate::windows_name::is_single_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid fixture asset name",
            ));
        }
        let source = self.0.join(name);
        if !super::ordinary_file(&source)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fixture asset is not a regular file",
            ));
        }
        copy_limited(File::open(source)?, dest, max_bytes)
    }
}

#[cfg(windows)]
pub(crate) struct GitHubSource;

#[cfg(windows)]
impl ReleaseSource for GitHubSource {
    fn list_releases(&self) -> io::Result<Vec<Release>> {
        let temp = tempfile::tempdir()?;
        let dest = temp.path().join(RELEASES_JSON);
        curl(
            "https://api.github.com/repos/momota1029/expos/releases?per_page=30",
            &dest,
            MAX_METADATA_BYTES,
        )
        .map_err(|error| unreadable_metadata(RELEASES_JSON, "download failed", error))?;
        let bytes = read_limited(&dest, MAX_METADATA_BYTES)
            .map_err(|error| unreadable_metadata(RELEASES_JSON, "read failed", error))?;
        parse_metadata_json(RELEASES_JSON, &bytes)
    }

    fn fetch_asset(&self, url: &str, dest: &Path, max_bytes: u64) -> io::Result<()> {
        if !valid_asset_url(url) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "untrusted GitHub asset URL",
            ));
        }
        curl(url, dest, max_bytes)
    }
}

#[cfg(any(windows, test))]
fn valid_asset_url(url: &str) -> bool {
    if url.bytes().any(|b| b <= b' ' || b == 127 || b == b'\\') {
        return false;
    }
    url.strip_prefix("https://")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(host, path)| {
            [
                "github.com",
                "objects.githubusercontent.com",
                "release-assets.githubusercontent.com",
            ]
            .iter()
            .any(|allowed| host.eq_ignore_ascii_case(allowed))
                && !path.is_empty()
        })
}

#[cfg(windows)]
fn curl(url: &str, dest: &Path, max_bytes: u64) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    // PATH 上の別の curl や利用者の curl 設定で実行先・引数が変わらないようにする。
    let exe = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SystemRoot is missing"))?
        .join("System32")
        .join("curl.exe");
    let mut child = Command::new(exe)
        .args([
            "--disable",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "5",
            "--connect-timeout",
            "10",
            "--max-time",
            "120",
            "--max-filesize",
            &max_bytes.to_string(),
            "--globoff",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: expos-updater",
            "--",
            url,
        ])
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let result = copy_limited(
        child.stdout.take().expect("piped curl output"),
        dest,
        max_bytes,
    );
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    result?;
    let status = status?;
    if !status.success() {
        let _ = fs::remove_file(dest);
        return Err(io::Error::other(format!("curl failed: {status}")));
    }
    Ok(())
}

pub(super) fn read_limited(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update metadata exceeds its size limit",
        ));
    }
    Ok(bytes)
}

fn copy_limited(mut input: impl Read, dest: &Path, max_bytes: u64) -> io::Result<()> {
    let mut output = File::options().write(true).create_new(true).open(dest)?;
    let result = (|| {
        io::copy(&mut input.by_ref().take(max_bytes), &mut output)?;
        if input.read(&mut [0])? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update asset exceeds its size limit",
            ));
        }
        Ok(())
    })();
    drop(output);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

pub(super) fn check(
    source: &dyn ReleaseSource,
    installed: &str,
    choice: Channel,
    switch: bool,
) -> io::Result<Option<Offer>> {
    let installed_version = VersionDir::from_version(installed)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid running version"))?;
    let mut releases = source.list_releases()?;
    let candidates = selection::candidates(&installed_version, choice, switch, &releases);
    if candidates.is_empty() {
        return Ok(None);
    }
    let temp = tempfile::tempdir()?;
    let mut failure = None;
    for (version, index) in candidates {
        let release = &mut releases[index];
        let assets = selection::release_assets(release).expect("filtered release");
        let metadata = (|| -> io::Result<_> {
            let manifest = fetch_metadata(
                source,
                &assets.manifest.browser_download_url,
                temp.path(),
                CHANNEL_JSON,
            )?;
            let manifest = parse_metadata_json(CHANNEL_JSON, &manifest)?;
            let sums = fetch_metadata(
                source,
                &assets.sums.browser_download_url,
                temp.path(),
                "SHA256SUMS",
            )?;
            Ok(ReleaseMetadata {
                manifest,
                sha256sums: sums,
            })
        })();
        match metadata {
            Ok(metadata) => release.metadata = Some(metadata),
            Err(error) => {
                failure.get_or_insert(error);
                continue;
            }
        }
        if selection::offer_for(&installed_version, version, release).is_some() {
            return Ok(if switch {
                selection::pick_update(installed, choice, std::slice::from_ref(release))
            } else {
                selection::pick(installed, choice, false, std::slice::from_ref(release))
            });
        }
    }
    failure.map_or(Ok(None), Err)
}

fn fetch_metadata(
    source: &dyn ReleaseSource,
    url: &str,
    dir: &Path,
    name: &str,
) -> io::Result<Vec<u8>> {
    let dest = dir.join(name);
    source
        .fetch_asset(url, &dest, MAX_METADATA_BYTES)
        .map_err(|error| unreadable_metadata(name, "download failed", error))?;
    let result = read_limited(&dest, MAX_METADATA_BYTES)
        .map_err(|error| unreadable_metadata(name, "read failed", error));
    fs::remove_file(dest)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZIP: &str = "expos-v2.0.0-windows-x86_64.zip";
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn write_release_fixture(dir: &Path, prefix: &[u8]) {
        let assets = [
            ZIP,
            "expos-v2.0.0-source-vendored.zip",
            "SHA256SUMS",
            CHANNEL_JSON,
        ]
        .map(|name| serde_json::json!({"name": name, "browser_download_url": name, "size": 1}));
        let releases = serde_json::json!([{"tag_name": "v2.0.0", "assets": assets}]);
        let manifest = serde_json::json!({
            "channel": "stable", "version": "2.0.0", "zip": ZIP, "sha256": HASH,
        });
        for (name, contents) in [
            (RELEASES_JSON, releases.to_string()),
            (CHANNEL_JSON, manifest.to_string()),
            ("SHA256SUMS", format!("{HASH}  {ZIP}\n")),
        ] {
            fs::write(dir.join(name), [prefix, contents.as_bytes()].concat()).unwrap();
        }
    }

    #[test]
    fn accepts_bom_in_release_metadata_and_still_checks_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write_release_fixture(dir.path(), UTF8_BOM);
        let source = DirSource(dir.path().into());
        let offer = check(&source, "1.0.0", Channel::Stable, false)
            .unwrap()
            .unwrap();
        assert_eq!(offer.tag, "v2.0.0");
        assert_eq!(offer.sha256, HASH);

        let sums = dir.path().join("SHA256SUMS");
        let mut bytes = fs::read(&sums).unwrap();
        bytes[UTF8_BOM.len()] = b'b';
        fs::write(sums, bytes).unwrap();
        assert!(
            check(&source, "1.0.0", Channel::Stable, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn html_in_either_json_document_returns_the_friendly_error() {
        let dir = tempfile::tempdir().unwrap();
        let source = DirSource(dir.path().into());
        for document in [RELEASES_JSON, CHANNEL_JSON] {
            write_release_fixture(dir.path(), b"");
            fs::write(dir.path().join(document), b"\xef\xbb\xbf \n<!DOCTYPE html>").unwrap();
            let error = check(&source, "1.0.0", Channel::Stable, false).unwrap_err();
            assert!(error.get_ref().unwrap().is::<UpdateInformationError>());
            assert_eq!(error.to_string(), "Couldn't read the update information");
        }
    }

    #[test]
    fn only_accepts_https_on_official_asset_hosts() {
        for value in [
            "https://github.com/momota1029/expos/releases/download/v1.2.3/expos.zip",
            "https://objects.githubusercontent.com/asset",
            "https://release-assets.githubusercontent.com/asset?sig=foo",
        ] {
            assert!(valid_asset_url(value), "{value}");
        }
        for value in [
            "http://github.com/asset",
            "https://github.com.evil.test/asset",
            "https://github.com@evil.test/asset",
            "https://evil.test\\@github.com/asset",
            "https://github.com:444/asset",
        ] {
            assert!(!valid_asset_url(value), "{value}");
        }
    }

    #[test]
    fn dir_source_refuses_oversized_assets_and_cleans_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let source = DirSource(dir.path().into());
        fs::write(dir.path().join("asset.zip"), b"bytes").unwrap();
        let dest = dir.path().join("dest.zip");
        assert!(source.fetch_asset("asset.zip", &dest, 4).is_err());
        assert!(!dest.exists());
        source.fetch_asset("asset.zip", &dest, 5).unwrap();
        assert_eq!(fs::read(dest).unwrap(), b"bytes");
    }
}
