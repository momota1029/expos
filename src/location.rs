//! ビューが今どこにいるか。
//!
//! filesystem の path とは限らない。Explorer の PC や Shell root のように path を持たない場所があるので、
//! `PathBuf` をそのまま持ち回らない。path が要る側は [`Location::path`] を通し、`Location` の
//! まま filesystem の API へ渡さない（`ThisPc` を path として読む事故を型で防ぐ）。

use std::path::{Path, PathBuf};

/// 名前付き Shell root。profile をまたいで意味を保つ入口を表す。
/// Child location は session に opaque parsing identity と表示名を値として保存する。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShellRoot {
    Libraries,
    Network,
    RecycleBin,
}

impl ShellRoot {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Libraries => "Libraries",
            Self::Network => "Network",
            Self::RecycleBin => "Recycle Bin",
        }
    }

    pub const fn identity(self) -> &'static str {
        match self {
            Self::Libraries => "libraries",
            Self::Network => "network",
            Self::RecycleBin => "recycle-bin",
        }
    }
}

/// ビューの location。
#[derive(Clone, Debug)]
pub enum Location {
    /// 列挙できる実在の場所。
    Path(PathBuf),
    /// Ordinary ZIP contents. `components` is a validated archive-relative address.
    Archive {
        source: PathBuf,
        components: Vec<String>,
    },
    /// Explorer の PC。drive や特殊フォルダを並べる仮想の場所で、自分では path を持たない。
    ///
    /// 行の1つを開いた時点で `Path` へ移る。まだ UI から到達できないので作られない
    /// （[PC decision](../docs/decisions/2026-09-19-pc-and-special-folders.md) の Step 3）。
    #[allow(dead_code)]
    ThisPc,
    /// filesystem の path ではなく Shell namespace で読む名前付き root。
    ShellRoot(ShellRoot),
    /// Shell root 配下で Shell が返した folder。
    ///
    /// `parsing_name` は Shell が発行する desktop-absolute parsing identity であり、
    /// ユーザー向けの `display_name` とは別に持つ。表示名の変更を行き先の識別に使わない。
    ShellItem {
        root: ShellRoot,
        parsing_name: String,
        display_name: String,
    },
}

impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Path(left), Self::Path(right)) => left == right,
            (
                Self::Archive {
                    source: left_source,
                    components: left_components,
                },
                Self::Archive {
                    source: right_source,
                    components: right_components,
                },
            ) => left_source == right_source && left_components == right_components,
            (Self::ThisPc, Self::ThisPc) => true,
            (Self::ShellRoot(left), Self::ShellRoot(right)) => left == right,
            (
                Self::ShellItem {
                    parsing_name: left, ..
                },
                Self::ShellItem {
                    parsing_name: right,
                    ..
                },
            ) => left == right,
            _ => false,
        }
    }
}

impl Eq for Location {}

impl Location {
    pub fn display_label(&self) -> Option<String> {
        match self {
            Self::Archive { source, components } => {
                Some(components.last().cloned().unwrap_or_else(|| {
                    source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| source.display().to_string())
                }))
            }
            _ => None,
        }
    }

    pub fn display_address(&self) -> Option<String> {
        match self {
            Self::Archive { source, components } => Some(if components.is_empty() {
                format!("{}!", source.display())
            } else {
                format!("{}!/{}", source.display(), components.join("/"))
            }),
            _ => None,
        }
    }

    /// path として扱えるときだけ返す。virtual root は `None`。
    ///
    /// 「path が無い」を空 path で代用しない。空 path はカレントディレクトリとして解決され、
    /// 一覧が黙って別の場所を指す。
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Archive { .. } | Self::ThisPc | Self::ShellRoot(_) | Self::ShellItem { .. } => {
                None
            }
        }
    }

    /// filesystem watch / F5 の対象ではない Shell namespace か。
    pub fn is_virtual(&self) -> bool {
        matches!(
            self,
            Self::ThisPc | Self::ShellRoot(_) | Self::ShellItem { .. }
        )
    }

    /// presentation が virtual row を対応付けるための stable identity。
    ///
    /// path の代用品ではない。Shell item は display name ではなく parsing identity を使う。
    pub fn virtual_identity(&self) -> Option<String> {
        match self {
            Self::ThisPc => Some("this-pc".into()),
            Self::ShellRoot(root) => Some(root.identity().into()),
            Self::ShellItem { parsing_name, .. } => Some(parsing_name.clone()),
            Self::Path(_) => None,
            Self::Archive { .. } => None,
        }
    }
}

impl From<PathBuf> for Location {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_location_exposes_its_path() {
        let location = Location::from(PathBuf::from(r"C:\work"));
        assert_eq!(location.path(), Some(Path::new(r"C:\work")));
    }

    /// PC は path を持たない。空 path で代用しない。
    #[test]
    fn this_pc_has_no_path() {
        assert_eq!(Location::ThisPc.path(), None);
    }

    #[test]
    fn shell_roots_are_not_filesystem_paths() {
        for root in [
            ShellRoot::Libraries,
            ShellRoot::Network,
            ShellRoot::RecycleBin,
        ] {
            assert!(Location::ShellRoot(root).is_virtual());
            assert_eq!(Location::ShellRoot(root).path(), None);
        }
    }

    #[test]
    fn shell_item_identity_does_not_depend_on_its_display_name() {
        let first = Location::ShellItem {
            root: ShellRoot::Libraries,
            parsing_name: "::{libraries}\\Documents".into(),
            display_name: "Documents".into(),
        };
        let renamed = Location::ShellItem {
            root: ShellRoot::Libraries,
            parsing_name: "::{libraries}\\Documents".into(),
            display_name: "Dokumente".into(),
        };
        assert_eq!(first, renamed);
    }

    #[test]
    fn archive_location_has_typed_identity_and_no_filesystem_path() {
        let location = Location::Archive {
            source: PathBuf::from(r"C:\work\files.zip"),
            components: vec!["nested".into()],
        };
        assert_eq!(location.path(), None);
        assert!(!location.is_virtual());
        assert_eq!(location.virtual_identity(), None);
    }
}
