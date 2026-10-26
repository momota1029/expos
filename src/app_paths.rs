//! v6 がファイルを置く場所。path を組み立てるのはここだけ。
//!
//! `session_store` や `keymap` がそれぞれ自前で root を組むと、root を変えたときに
//! 片方だけ古い場所を見る。v5 の設定ディレクトリは流用しない。

use std::path::PathBuf;

/// `%LOCALAPPDATA%/gpui-filer/v6`。環境変数が無ければ `None`。
///
/// 環境変数が無いときに path を推測しない。書けない場所へ書きにいかないため。
pub(super) fn root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    // `join` を分けると、診断に出す path の区切りが混ざらない。
    Some(root.join("gpui-filer").join("v6"))
}

/// セッションの保存。`session_store` が置換と検証を担う。
pub(super) fn session_file() -> Option<PathBuf> {
    Some(root()?.join("session").join("session-v1.json"))
}

/// 外部 keymap。ユーザーが置く。無ければ既定の binding だけで動く。
pub(super) fn keymap_file() -> Option<PathBuf> {
    Some(root()?.join("keymap.toml"))
}
