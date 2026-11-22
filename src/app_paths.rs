//! アプリが使う設定・状態・キャッシュの保存先。v5 の設定ディレクトリは流用しない。

#[cfg(not(windows))]
use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(windows)]
pub(crate) use windows_data::{data_root, prepare_startup_data};

pub(super) fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return data_root();
    #[cfg(not(windows))]
    xdg_dir("XDG_CONFIG_HOME", ".config", std::env::var_os)
}

pub(super) fn state_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return data_root();
    #[cfg(not(windows))]
    xdg_dir("XDG_STATE_HOME", ".local/state", std::env::var_os)
}

pub(super) fn cache_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return data_root();
    #[cfg(not(windows))]
    xdg_dir("XDG_CACHE_HOME", ".cache", std::env::var_os)
}

#[cfg(not(windows))]
fn xdg_dir(
    variable: &'static str,
    home_suffix: &str,
    lookup: impl Fn(&'static str) -> Option<OsString>,
) -> Option<PathBuf> {
    // XDG の相対パスは無視し、HOME も相対なら作業ディレクトリに書かない。
    let absolute = |name| {
        lookup(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let base =
        absolute(variable).or_else(|| absolute("HOME").map(|home| home.join(home_suffix)))?;
    Some(base.join("expos"))
}

/// セッションの保存。`session_store` が置換と検証を担う。
pub(super) fn session_file() -> Option<PathBuf> {
    Some(state_dir()?.join("session").join("session-v1.json"))
}

/// 外部 keymap。ユーザーが置く。無ければ既定の binding だけで動く。
pub(super) fn keymap_file() -> Option<PathBuf> {
    Some(config_dir()?.join("keymap.toml"))
}

#[cfg(any(windows, test))]
mod windows_data {
    use crate::channel::Channel;
    use crate::install_info;
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    #[cfg(windows)]
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, Eq, PartialEq)]
    struct DataPaths {
        root: PathBuf,
        migration_source: Option<PathBuf>,
    }

    #[cfg(windows)]
    fn paths() -> Option<&'static DataPaths> {
        static PATHS: OnceLock<Option<DataPaths>> = OnceLock::new();
        PATHS
            .get_or_init(|| {
                resolve(
                    std::env::var_os,
                    || {
                        Some((
                            install_info::install_root()?,
                            install_info::version_dir()?,
                            std::env::current_exe().ok()?,
                        ))
                    },
                    env!("CARGO_PKG_VERSION"),
                    Channel::current(),
                )
            })
            .as_ref()
    }

    #[cfg(windows)]
    pub(crate) fn data_root() -> Option<PathBuf> {
        Some(paths()?.root.clone())
    }

    #[cfg(windows)]
    pub(crate) fn prepare_startup_data() {
        let Some(paths) = paths() else { return };
        if std::env::var_os("GPUI_FILER_PROFILE").is_some() {
            let _ = writeln!(io::stderr().lock(), "[v6][data] {}", paths.root.display());
        }
        if let Some(source) = &paths.migration_source
            && let Err(error) = migrate(source, &paths.root)
        {
            let _ = writeln!(
                io::stderr().lock(),
                "[v6][data] could not migrate {} to {}: {error}",
                source.display(),
                paths.root.display(),
            );
        }
    }

    fn resolve(
        lookup: impl Fn(&'static str) -> Option<OsString>,
        installation: impl FnOnce() -> Option<(PathBuf, PathBuf, PathBuf)>,
        version: &str,
        channel: Channel,
    ) -> Option<DataPaths> {
        if let Some(root) = lookup("EXPOS_DATA_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            return Some(DataPaths {
                root,
                migration_source: None,
            });
        }

        let local = PathBuf::from(lookup("LOCALAPPDATA")?);
        let legacy = local.join("gpui-filer").join("v6");
        if let Some((install_root, version_dir, exe)) = installation()
            && install_root.is_absolute()
            && install_info::is_running_version_dir(&install_root, &version_dir, version)
            && let Some(exe_dir) = exe.parent().filter(|dir| dir.is_absolute())
            && same_directory(exe_dir, &version_dir)
        {
            return Some(DataPaths {
                root: local.join("expos").join("data").join(channel.as_str()),
                migration_source: Some(legacy),
            });
        }
        Some(DataPaths {
            root: legacy,
            migration_source: None,
        })
    }

    fn same_directory(exe_dir: &Path, version_dir: &Path) -> bool {
        // Windows では起動時と取得時で絶対パスの表記が異なることがある。
        exe_dir == version_dir
            || fs::canonicalize(exe_dir)
                .is_ok_and(|exe_dir| fs::canonicalize(version_dir).is_ok_and(|dir| dir == exe_dir))
    }

    fn migrate(source: &Path, destination: &Path) -> io::Result<()> {
        if destination.try_exists()? || !source.try_exists()? {
            return Ok(());
        }

        let parent = destination
            .parent()
            .expect("channel data root has a parent");
        let temp = destination.with_extension(format!("tmp-{}", std::process::id()));
        fs::create_dir_all(parent)?;
        fs::create_dir(&temp)?;

        let result: io::Result<bool> = (|| {
            copy_tree(source, &temp, true)?;
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_secs();
            writeln!(
                File::create_new(temp.join(".migrated-from"))?,
                "{}\nunix_timestamp={timestamp}",
                source.display(),
            )?;
            // 完成したコピーだけを公開する。先に公開されたディレクトリは上書きしない。
            if destination.try_exists()? {
                return Ok(false);
            }
            fs::rename(&temp, destination)?;
            Ok(true)
        })();

        let error = match result {
            Ok(true) => return Ok(()),
            Ok(false) => None,
            Err(error) => Some(error),
        };
        match fs::remove_dir_all(&temp) {
            Err(cleanup) if cleanup.kind() != io::ErrorKind::NotFound => Err(match error {
                Some(error) => {
                    io::Error::other(format!("{error}; removing {}: {cleanup}", temp.display()))
                }
                None => cleanup,
            }),
            _ => error.map_or(Ok(()), Err),
        }
    }

    fn copy_tree(source: &Path, destination: &Path, is_root: bool) -> io::Result<()> {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            let lower = name.to_string_lossy().to_ascii_lowercase();
            if (is_root && matches!(lower.as_str(), "cache" | "thumbnails"))
                || lower.ends_with(".lock")
                || lower.ends_with(".tmp")
                || lower.contains(".tmp-")
            {
                continue;
            }

            let metadata = entry.metadata()?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                    continue;
                }
            }
            let target = destination.join(name);
            if metadata.is_dir() {
                fs::create_dir(&target)?;
                copy_tree(&entry.path(), &target, false)?;
            } else if metadata.is_file() {
                // 読み取り専用属性は引き継がず、失敗時にも作業ディレクトリを消せるようにする。
                io::copy(
                    &mut File::open(entry.path())?,
                    &mut File::create_new(target)?,
                )?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn absolute_override_wins_without_local_app_data_or_launcher_paths() {
            let fixture = tempfile::tempdir().unwrap();
            let custom = fixture.path().join("custom");
            let paths = resolve(
                |name| match name {
                    "EXPOS_DATA_DIR" => Some(custom.as_os_str().to_owned()),
                    _ => panic!("absolute override should resolve immediately"),
                },
                || panic!("absolute override should not need installation metadata"),
                "0.1.1-beta.7",
                Channel::Beta,
            );
            assert_eq!(
                paths,
                Some(DataPaths {
                    root: custom,
                    migration_source: None
                })
            );
        }

        #[test]
        fn launched_versions_use_distinct_data_roots_under_local_app_data() {
            let fixture = tempfile::tempdir().unwrap();
            let local = fixture.path().join("local");
            let install = fixture.path().join("custom-install");
            let mut versions = vec![
                ("0.2.0", "v0.2.0", "stable"),
                ("0.1.1-beta.7", "v0.1.1-beta.7", "beta"),
            ];
            if cfg!(debug_assertions) {
                versions.extend([
                    ("0.2.0", "v0.2.0-alpha.dev.20261123T093012-0", "stable"),
                    ("0.1.1-beta.7", "v0.1.1-alpha.dev.20261123T093012-0", "beta"),
                ]);
            }
            for (version, directory, name) in versions {
                let version_dir = install.join(directory);
                fs::create_dir_all(&version_dir).unwrap();
                let exe = version_dir.canonicalize().unwrap().join("expos.exe");
                let paths = resolve(
                    |name| match name {
                        "EXPOS_DATA_DIR" => Some("relative-path".into()),
                        "LOCALAPPDATA" => Some(local.as_os_str().to_owned()),
                        _ => None,
                    },
                    || Some((install.clone(), version_dir, exe)),
                    version,
                    Channel::of_version(version),
                )
                .unwrap();
                assert_eq!(paths.root, local.join("expos").join("data").join(name));
                assert_eq!(
                    paths.migration_source,
                    Some(local.join("gpui-filer").join("v6"))
                );
            }
        }

        #[test]
        fn direct_and_inconsistent_launches_use_legacy_paths() {
            let fixture = tempfile::tempdir().unwrap();
            let local = fixture.path();
            let install = local.join("expos");
            let version = install.join("v0.2.0");
            let nested_version = install.join("unexpected").join("v0.2.0");
            let wrong_version = install.join("v0.1.1-beta.7");
            let wrong_dev = install.join("v0.1.1-alpha.dev.20261123T093012-0");
            let good_exe = version.join("expos.exe");

            for installation in [
                None,
                Some((install.clone(), version.clone(), local.join("expos.exe"))),
                Some((
                    install.clone(),
                    version.clone(),
                    version.join("tools").join("expos.exe"),
                )),
                Some((
                    install.clone(),
                    wrong_version.clone(),
                    wrong_version.join("expos.exe"),
                )),
                Some((
                    install.clone(),
                    wrong_dev.clone(),
                    wrong_dev.join("expos.exe"),
                )),
                Some((
                    install.clone(),
                    nested_version.clone(),
                    nested_version.join("expos.exe"),
                )),
                Some((PathBuf::from("relative"), version.clone(), good_exe)),
            ] {
                let paths = resolve(
                    |name| (name == "LOCALAPPDATA").then(|| local.as_os_str().to_owned()),
                    || installation,
                    "0.2.0",
                    Channel::Stable,
                )
                .unwrap();
                assert_eq!(paths.root, local.join("gpui-filer").join("v6"));
                assert_eq!(paths.migration_source, None);
            }
        }

        #[test]
        fn missing_local_app_data_without_absolute_override_means_no_path() {
            assert_eq!(resolve(|_| None, || None, "0.2.0", Channel::Stable), None);
        }

        #[test]
        fn migration_copies_each_channel_from_the_legacy_tree() {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("legacy");
            let stable = fixture.path().join("data").join("stable");
            let beta = stable.with_file_name("beta");
            migrate(&source, &beta).unwrap();
            assert!(!beta.exists());

            let files = [
                ("session/session-v1.json", true),
                ("session/session-v1.json.bak", true),
                ("session/session-v1.json.1.tmp", false),
                ("session/session-v1.json.lock", false),
                ("folder-view-preferences-v1.json", true),
                ("operations/journal-v1.json", true),
                ("operations/.journal-v1.json.1.2.tmp", false),
                ("cache/object", false),
                ("thumbnails/v2/preview", false),
                ("themes/custom.toml", true),
                (".lock", false),
                ("keymap.toml", true),
            ];
            for (name, _) in files {
                let file = source.join(name);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, name).unwrap();
            }
            migrate(&source, &beta).unwrap();
            fs::write(beta.join("keymap.toml"), "beta edit").unwrap();
            migrate(&source, &beta).unwrap();
            migrate(&source, &stable).unwrap();

            for (name, copied) in files {
                assert_eq!(fs::read_to_string(source.join(name)).unwrap(), name);
                if copied {
                    assert_eq!(fs::read_to_string(stable.join(name)).unwrap(), name);
                } else {
                    assert!(!stable.join(name).exists());
                    assert!(!beta.join(name).exists());
                }
            }
            assert_eq!(
                fs::read_to_string(beta.join("keymap.toml")).unwrap(),
                "beta edit"
            );
            assert!(!stable.join("cache").exists());
            assert!(!stable.join("thumbnails").exists());
            for destination in [stable, beta] {
                let marker = fs::read_to_string(destination.join(".migrated-from")).unwrap();
                let (path, timestamp) = marker.trim_end().split_once("\nunix_timestamp=").unwrap();
                assert_eq!(path, source.to_string_lossy());
                assert!(timestamp.parse::<u64>().unwrap() > 0);
            }
            assert!(!source.join(".migrated-from").exists());
        }

        #[test]
        fn migration_cleans_up_temporary_copy_on_failure() {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("legacy");
            let destination = fixture.path().join("data").join("stable");
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("keymap.toml"), "legacy").unwrap();
            // マーカーの作成を衝突させ、ファイルをコピーした後の失敗を再現する。
            fs::create_dir(source.join(".migrated-from")).unwrap();

            assert!(migrate(&source, &destination).is_err());
            assert_eq!(
                fs::read_dir(destination.parent().unwrap()).unwrap().count(),
                0
            );
            assert_eq!(
                fs::read_to_string(source.join("keymap.toml")).unwrap(),
                "legacy"
            );
            assert!(source.join(".migrated-from").is_dir());
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn absolute_xdg_path_overrides_home() {
        let path = xdg_dir("XDG_CONFIG_HOME", ".config", |name| match name {
            "XDG_CONFIG_HOME" => Some("/custom/config".into()),
            "HOME" => panic!("absolute XDG path should not need HOME"),
            _ => None,
        });
        assert_eq!(path, Some(PathBuf::from("/custom/config/expos")));
    }

    #[test]
    fn missing_empty_and_relative_xdg_paths_use_home_defaults() {
        for (variable, suffix, expected) in [
            ("XDG_CONFIG_HOME", ".config", "/home/user/.config/expos"),
            (
                "XDG_STATE_HOME",
                ".local/state",
                "/home/user/.local/state/expos",
            ),
            ("XDG_CACHE_HOME", ".cache", "/home/user/.cache/expos"),
        ] {
            for xdg_value in [None, Some(""), Some("relative")] {
                let path = xdg_dir(variable, suffix, |name| match name {
                    "HOME" => Some("/home/user".into()),
                    name if name == variable => xdg_value.map(OsString::from),
                    _ => None,
                });
                assert_eq!(path, Some(PathBuf::from(expected)));
            }
        }
    }

    #[test]
    fn no_absolute_base_means_no_path() {
        for home in [None, Some(""), Some("relative")] {
            let path = xdg_dir("XDG_STATE_HOME", ".local/state", |name| match name {
                "XDG_STATE_HOME" => Some("relative/state".into()),
                "HOME" => home.map(OsString::from),
                _ => None,
            });
            assert_eq!(path, None);
        }
    }
}
