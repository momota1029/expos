use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{Channel, without_utf8_bom};

const FILE: &str = "update-settings-v1.json";
const BAD_PREFIX: &str = "update-settings-v1.json.bad-";
const KEEP_BAD_FILES: usize = 3;
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Settings {
    #[serde(with = "channel")]
    pub channel: Channel,
    pub last_check_unix: Option<u64>,
    // v1 では skip UI を提供しないため常に null。
    pub skipped_version: Option<()>,
    #[serde(default = "enabled")]
    pub automatic: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            channel: Channel::current(),
            last_check_unix: None,
            skipped_version: None,
            automatic: true,
        }
    }
}

impl Settings {
    pub(super) fn load(root: &Path) -> io::Result<Self> {
        let path = root.join(FILE);
        let error = match super::source::read_limited(&path, 4096) {
            Ok(bytes) => match serde_json::from_slice(without_utf8_bom(bytes.trim_ascii_start())) {
                Ok(settings) => return Ok(settings),
                Err(error) => io::Error::new(io::ErrorKind::InvalidData, error),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => error,
            Err(error) => return Err(error),
        };
        match quarantine(root, &path) {
            Ok(backup) => eprintln!(
                "[expos][update] invalid {FILE}, using defaults, moved to {}: {error}",
                backup.display()
            ),
            Err(rename_error) => eprintln!(
                "[expos][update] invalid {FILE}, using defaults: {error} (backup failed: {rename_error})"
            ),
        }
        Ok(Self::default())
    }

    pub(super) fn save(self, root: &Path) -> io::Result<()> {
        fs::create_dir_all(root)?;
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        serde_json::to_writer(&mut temp, &self)?;
        writeln!(temp)?;
        temp.as_file().sync_all()?;
        temp.into_temp_path()
            .persist(root.join(FILE))
            .map_err(|error| error.error)
    }

    pub(super) fn auto_due(self, now: u64) -> bool {
        self.automatic
            && self
                .last_check_unix
                .is_none_or(|last| now.saturating_sub(last) >= CHECK_INTERVAL.as_secs())
    }

    pub(super) fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

fn quarantine(root: &Path, path: &Path) -> io::Result<PathBuf> {
    // 同じ秒に再検出しても、前のバックアップを上書きしない。
    let mut timestamp = Settings::now();
    let backup = loop {
        let candidate = root.join(format!("{BAD_PREFIX}{timestamp}"));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => timestamp += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => break candidate,
            Err(error) => return Err(error),
        }
    };
    fs::rename(path, &backup)?;

    if let Ok(entries) = fs::read_dir(root) {
        let mut older = entries
            .flatten()
            .filter_map(|entry| {
                let timestamp = entry
                    .file_name()
                    .to_str()?
                    .strip_prefix(BAD_PREFIX)?
                    .parse::<u64>()
                    .ok()?;
                let path = entry.path();
                (path != backup && entry.file_type().ok()?.is_file()).then_some((timestamp, path))
            })
            .collect::<Vec<_>>();
        older.sort_unstable();
        let excess = older.len().saturating_sub(KEEP_BAD_FILES - 1);
        for (_, path) in older.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(backup)
}

fn enabled() -> bool {
    true
}

mod channel {
    use super::Channel;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(channel: &Channel, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(channel.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Channel, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "stable" => Ok(Channel::Stable),
            "beta" => Ok(Channel::Beta),
            _ => Err(serde::de::Error::custom("unknown update channel")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_bom_prefixed_settings_with_leading_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let json = b" {\"channel\":\"stable\",\"last_check_unix\":42,\"automatic\":false}";
        for prefix in [b"\xef\xbb\xbf\r\n".as_slice(), b" \t\xef\xbb\xbf", b" \r\n"] {
            let bytes = [prefix, json.as_slice()].concat();
            fs::write(dir.path().join(FILE), bytes).unwrap();
            assert_eq!(
                Settings::load(dir.path()).unwrap(),
                Settings {
                    channel: Channel::Stable,
                    last_check_unix: Some(42),
                    automatic: false,
                    skipped_version: None,
                }
            );
        }
    }

    #[test]
    fn corrupt_or_unknown_settings_are_quarantined_with_defaults_and_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for timestamp in 1..=3 {
            fs::write(root.join(format!("{BAD_PREFIX}{timestamp}")), b"old").unwrap();
        }
        let unrelated = root.join(format!("{BAD_PREFIX}notes"));
        fs::write(&unrelated, b"notes").unwrap();

        let path = root.join(FILE);
        for bytes in [b"garbage".as_slice(), br#"{"channel":"stable","schema":2}"#] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(Settings::load(root).unwrap(), Settings::default());
            assert!(!path.exists());
            let backups = fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path != &unrelated)
                .collect::<Vec<_>>();
            assert_eq!(backups.len(), KEEP_BAD_FILES);
            assert!(backups.iter().all(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix(BAD_PREFIX)
                    .unwrap()
                    .parse::<u64>()
                    .is_ok()
            }));
            assert!(backups.iter().any(|path| fs::read(path).unwrap() == bytes));
        }
        assert!(!root.join(format!("{BAD_PREFIX}1")).exists());
        assert!(!root.join(format!("{BAD_PREFIX}2")).exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"notes");
        Settings::default().save(root).unwrap();
        assert_eq!(Settings::load(root).unwrap(), Settings::default());
    }

    #[test]
    fn round_trip_and_twelve_hour_boundary() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), Settings::default());
        let mut settings = Settings {
            channel: Channel::Stable,
            last_check_unix: Some(100),
            ..Settings::default()
        };
        settings.save(dir.path()).unwrap();
        assert!(!settings.auto_due(100 + CHECK_INTERVAL.as_secs() - 1));
        assert!(settings.auto_due(100 + CHECK_INTERVAL.as_secs()));
        settings.automatic = false;
        settings.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), settings);
        assert!(!settings.auto_due(u64::MAX));
        let bytes = fs::read(dir.path().join(FILE)).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["skipped_version"].is_null());
    }
}
