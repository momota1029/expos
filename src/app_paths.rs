//! アプリが使う設定・状態・キャッシュの保存先。v5 の設定ディレクトリは流用しない。

#[cfg(not(windows))]
use std::ffi::OsString;
use std::path::PathBuf;

/// `%LOCALAPPDATA%/gpui-filer/v6`。環境変数が無ければ `None`。
#[cfg(windows)]
fn windows_root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    // `join` を分けると、診断に出す path の区切りが混ざらない。
    Some(root.join("gpui-filer").join("v6"))
}

pub(super) fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return windows_root();
    #[cfg(not(windows))]
    xdg_dir("XDG_CONFIG_HOME", ".config", std::env::var_os)
}

pub(super) fn state_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return windows_root();
    #[cfg(not(windows))]
    xdg_dir("XDG_STATE_HOME", ".local/state", std::env::var_os)
}

pub(super) fn cache_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return windows_root();
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
