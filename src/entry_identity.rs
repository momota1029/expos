//! 一覧の項目を並べ替えや再列挙をまたいで指すためのキー。
//!
//! 選択とフォーカスは index ではなくこのキーで持つ。index は表示順に依存するので、
//! sort や refresh で並びが変わると別の項目を指してしまう。
//!
//! ここでいう identity は **UI の対応キー**であって、filesystem object の同一性では
//! ない。同じ path / size / mtime でも外部で差し替えられる。削除や上書きを認可する
//! 判断には使わない（`docs/FILE_OPERATIONS.md` を参照）。

use crate::directory::EntryKind;
use std::path::PathBuf;

/// 表示順に依存しない項目の識別子。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKey {
    /// 実ファイル。lossless な full path と kind の組。
    ///
    /// 表示用の `name` や小文字化した文字列から作らない。Windows の case や
    /// junction alias を独自に同一視しないため、path はそのまま使う。
    Path { path: PathBuf, kind: EntryKind },
    /// Archive member identity never doubles as a filesystem path.
    Archive {
        source: PathBuf,
        components: Vec<String>,
        kind: EntryKind,
    },
    /// demo の合成項目。path を持たないので生成時の安定 ID を使う。
    Demo(u64),
    /// Shell namespace の行。表示名ではなく Shell adapter が返した identity を使う。
    Virtual(String),
}

impl EntryKey {
    pub fn path(path: PathBuf, kind: EntryKind) -> Self {
        Self::Path { path, kind }
    }

    pub fn demo(index: u64) -> Self {
        Self::Demo(index)
    }

    pub fn virtual_item(identity: String) -> Self {
        Self::Virtual(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_keys_distinguish_kind() {
        let path = PathBuf::from(r"C:\work\thing");
        assert_ne!(
            EntryKey::path(path.clone(), EntryKind::Folder),
            EntryKey::path(path, EntryKind::File)
        );
    }

    #[test]
    fn path_keys_compare_by_exact_path() {
        // Windows の case を独自に同一視しない。OS が同じと見なすかは OS に任せる。
        assert_ne!(
            EntryKey::path(PathBuf::from(r"C:\Work"), EntryKind::Folder),
            EntryKey::path(PathBuf::from(r"C:\work"), EntryKind::Folder)
        );
    }

    #[test]
    fn demo_keys_are_stable_and_distinct() {
        assert_eq!(EntryKey::demo(3), EntryKey::demo(3));
        assert_ne!(EntryKey::demo(3), EntryKey::demo(4));
    }

    #[test]
    fn virtual_keys_do_not_use_the_display_name() {
        assert_eq!(
            EntryKey::virtual_item("::{libraries}\\Documents".into()),
            EntryKey::virtual_item("::{libraries}\\Documents".into())
        );
    }
}
