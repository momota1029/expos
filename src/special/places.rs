use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use gpui_filer_resources::IconId;

use super::{PcEntry, PcEntryKind};
use crate::location::Location;
use crate::shell::Message;

const USER_DIRS: &[(&str, &str, IconId, Message)] = &[
    (
        "DESKTOP",
        "Desktop",
        IconId::FolderDesktop,
        Message::PcPlaceDesktop,
    ),
    (
        "DOCUMENTS",
        "Documents",
        IconId::FolderDocuments,
        Message::PcPlaceDocuments,
    ),
    (
        "DOWNLOAD",
        "Downloads",
        IconId::FolderDownloads,
        Message::PcPlaceDownloads,
    ),
    ("MUSIC", "Music", IconId::FolderMusic, Message::PcPlaceMusic),
    (
        "PICTURES",
        "Pictures",
        IconId::FolderPictures,
        Message::PcPlacePictures,
    ),
    (
        "VIDEOS",
        "Videos",
        IconId::FolderVideos,
        Message::PcPlaceVideos,
    ),
];

pub(super) fn place_label(icon: IconId) -> Option<Message> {
    if icon == IconId::FolderHome {
        return Some(Message::PcPlaceHome);
    }
    USER_DIRS
        .iter()
        .find_map(|&(_, _, place_icon, label)| (place_icon == icon).then_some(label))
}

pub(super) fn known_folders() -> Vec<PcEntry> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.as_ref().map(|home| home.join(".config")))
        .and_then(|dir| std::fs::read_to_string(dir.join("user-dirs.dirs")).ok())
        .unwrap_or_default();
    known_folder_rows(home.as_deref(), &config, Path::is_dir)
}

fn known_folder_rows(
    home: Option<&Path>,
    config: &str,
    is_dir: impl Fn(&Path) -> bool,
) -> Vec<PcEntry> {
    let configured = parse_user_dirs(config, home);
    let root = Path::new("/");
    let mut rows = Vec::new();
    if let Some(home) = home.filter(|&home| home != root && is_dir(home)) {
        rows.push(place("Home", home.to_path_buf(), IconId::FolderHome));
    }

    for &(key, name, icon, _) in USER_DIRS {
        let path = configured.get(key).map(PathBuf::as_path);
        // xdg-user-dirs は無効化されたディレクトリを HOME に設定する。
        if path.is_some() && path == home {
            continue;
        }
        let Some(path) = path
            .filter(|path| is_dir(path))
            .map(Path::to_path_buf)
            .or_else(|| home.map(|home| home.join(name)).filter(|path| is_dir(path)))
        else {
            continue;
        };
        // PC の row key は path なので、同じ行き先には一つだけ icon と名前を付ける。
        if path != root
            && !rows
                .iter()
                .any(|row| row.target.path() == Some(path.as_path()))
        {
            rows.push(place(name, path, icon));
        }
    }
    rows
}

fn place(name: &str, path: PathBuf, icon: IconId) -> PcEntry {
    PcEntry {
        name: name.into(),
        target: Location::Path(path),
        kind: PcEntryKind::Place(icon),
        source_identity: None,
    }
}

fn parse_user_dirs<'a>(config: &'a str, home: Option<&Path>) -> HashMap<&'a str, PathBuf> {
    config
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim().strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            Some((key, parse_user_dir(value.trim_start(), home)?))
        })
        .collect()
}

fn parse_user_dir(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    let value = value.strip_prefix('"')?;
    let home_relative = value
        .strip_prefix("$HOME")
        .filter(|rest| rest.starts_with(['/', '"']));
    let mut chars = home_relative.unwrap_or(value).chars();
    let mut path = String::new();

    // 設定をシェルに渡さず、二重引用符の escape と先頭の $HOME だけを解釈する。
    loop {
        match chars.next()? {
            '"' => break,
            '\\' if chars.as_str().starts_with(['"', '\\', '$', '`']) => {
                path.push(chars.next()?);
            }
            '$' | '`' | '\0' => return None,
            ch => path.push(ch),
        }
    }
    let tail = chars.as_str().trim_start();
    if !tail.is_empty() && !tail.starts_with('#') {
        return None;
    }

    if home_relative.is_some() {
        return Some(home?.join(path.trim_start_matches('/')));
    }
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_home_and_absolute_xdg_paths() {
        let config = r#"
            # XDG_DOCUMENTS_DIR="/ignored"
            XDG_DOCUMENTS_DIR="$HOME/My Documents"
            XDG_DOWNLOAD_DIR="/mnt/shared/Downloads"
            XDG_DESKTOP_DIR="$HOME/"
            XDG_PICTURES_DIR = "/photos/\$album/\"party\"/a\\b" # comment
        "#;
        let home = Path::new("/home/test");
        let dirs = parse_user_dirs(config, Some(home));
        assert_eq!(dirs["DOCUMENTS"], home.join("My Documents"));
        assert_eq!(dirs["DOWNLOAD"], Path::new("/mnt/shared/Downloads"));
        assert_eq!(dirs["DESKTOP"], home);
        assert_eq!(dirs["PICTURES"], Path::new(r#"/photos/$album/"party"/a\b"#));
    }

    #[test]
    fn ignores_relative_paths_malformed_lines_and_shell_expressions() {
        let config = r#"
            XDG_DOCUMENTS_DIR="relative"
            XDG_DOWNLOAD_DIR="/tmp/$(echo Downloads)"
            XDG_PICTURES_DIR="$HOME/Pictures" trailing
            XDG_MUSIC_DIR="/music
            XDG_VIDEOS_DIR=/videos
        "#;
        assert!(parse_user_dirs(config, Some(Path::new("/home/test"))).is_empty());

        let config = r#"
            XDG_DOCUMENTS_DIR="$HOME/Documents"
            XDG_PICTURES_DIR="/pictures"
        "#;
        let dirs = parse_user_dirs(config, None);
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs["PICTURES"], Path::new("/pictures"));
    }

    #[test]
    fn projects_existing_places_with_their_icons_and_one_filesystem_root() {
        let config = r#"
            XDG_DESKTOP_DIR="$HOME"
            XDG_DOCUMENTS_DIR="/shared"
            XDG_DOWNLOAD_DIR="/missing"
            XDG_MUSIC_DIR="/shared"
            XDG_VIDEOS_DIR="/"
        "#;
        let home = Path::new("/home/test");
        let folders = known_folder_rows(Some(home), config, |path| {
            [
                "/home/test",
                "/home/test/Desktop",
                "/home/test/Downloads",
                "/shared",
                "/",
            ]
            .iter()
            .any(|existing| path == Path::new(existing))
        });
        use crate::shell::{Locale, Translator};

        let rows = super::super::place_rows(folders.clone(), &Translator::new(Locale::English));
        let actual = rows
            .iter()
            .map(|row| (row.name.as_str(), row.target.path().unwrap(), row.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                ("Home", home, PcEntryKind::Place(IconId::FolderHome)),
                (
                    "Documents",
                    Path::new("/shared"),
                    PcEntryKind::Place(IconId::FolderDocuments)
                ),
                (
                    "Downloads",
                    Path::new("/home/test/Downloads"),
                    PcEntryKind::Place(IconId::FolderDownloads)
                ),
                ("File System", Path::new("/"), PcEntryKind::Drive),
            ]
        );
        let japanese = super::super::place_rows(folders, &Translator::new(Locale::Japanese));
        assert_eq!(
            japanese
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["ホーム", "ドキュメント", "ダウンロード", "ファイルシステム"]
        );
        assert!(
            japanese
                .iter()
                .zip(&rows)
                .all(|(ja, en)| ja.target == en.target)
        );
    }
}
