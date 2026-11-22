use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
#[cfg(windows)]
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::Channel;

const FILE: &str = "update-settings-v1.json";
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
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
        match super::source::read_limited(&root.join(FILE), 4096) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
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

    #[cfg(windows)]
    pub(super) fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
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
