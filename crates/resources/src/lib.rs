use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

pub const FLUENT11_LICENSE: &str = include_str!("../assets/licenses/fluent11/LICENSE");
pub const FLUENT11_AUTHORS: &str = include_str!("../assets/licenses/fluent11/AUTHORS.txt");

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IconId {
    Menu,
    Add,
    ZoomIn,
    ZoomOut,
    Close,
    Check,
    ChevronUp,
    ChevronDown,
    Back,
    Forward,
    Refresh,
    Search,
    Open,
    Copy,
    Cut,
    Paste,
    Rename,
    Delete,
    CopyPath,
    Properties,
    NewFolder,
    NewFile,
    CreateLink,
    SplitPane,
    FolderTree,
    Expand,
    ReopenTab,
    WorkspaceHistory,
    CloseOthers,
    Sort,
    Calendar,
    Rows,
    Tiles,
    Lock,
    Unlock,
    Warning,
    OperationCompleted,
    OperationFailed,
    OperationInProgress,
    OperationCancelled,
    Play,
    FolderFallbackGlyph,
    Libraries,
    Network,
    RecycleBin,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    WindowClose,
    File,
    FileText,
    FilePng,
    FileJpeg,
    FileRust,
    FileJavaScript,
    FilePython,
    FileMarkdown,
    FileToml,
    FileJson,
    FileYaml,
    Folder,
    FolderBack,
    FolderFront,
    FolderDesktop,
    FolderDocuments,
    FolderDownloads,
    FolderHome,
    FolderMusic,
    FolderPictures,
    FolderVideos,
    Link,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconSource {
    TextGlyph(char),
    Svg(&'static str),
    ColorSvg(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IconDefinition {
    pub name: &'static str,
    pub source: IconSource,
}

impl IconId {
    pub const ALL: &'static [Self] = &[
        Self::Menu,
        Self::Add,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::Close,
        Self::Check,
        Self::ChevronUp,
        Self::ChevronDown,
        Self::Back,
        Self::Forward,
        Self::Refresh,
        Self::Search,
        Self::Open,
        Self::Copy,
        Self::Cut,
        Self::Paste,
        Self::Rename,
        Self::Delete,
        Self::CopyPath,
        Self::Properties,
        Self::NewFolder,
        Self::NewFile,
        Self::CreateLink,
        Self::SplitPane,
        Self::FolderTree,
        Self::Expand,
        Self::ReopenTab,
        Self::WorkspaceHistory,
        Self::CloseOthers,
        Self::Sort,
        Self::Calendar,
        Self::Rows,
        Self::Tiles,
        Self::Lock,
        Self::Unlock,
        Self::Warning,
        Self::OperationCompleted,
        Self::OperationFailed,
        Self::OperationInProgress,
        Self::OperationCancelled,
        Self::Play,
        Self::FolderFallbackGlyph,
        Self::Libraries,
        Self::Network,
        Self::RecycleBin,
        Self::WindowMinimize,
        Self::WindowMaximize,
        Self::WindowRestore,
        Self::WindowClose,
        Self::File,
        Self::FileText,
        Self::FilePng,
        Self::FileJpeg,
        Self::FileRust,
        Self::FileJavaScript,
        Self::FilePython,
        Self::FileMarkdown,
        Self::FileToml,
        Self::FileJson,
        Self::FileYaml,
        Self::Folder,
        Self::FolderBack,
        Self::FolderFront,
        Self::FolderDesktop,
        Self::FolderDocuments,
        Self::FolderDownloads,
        Self::FolderHome,
        Self::FolderMusic,
        Self::FolderPictures,
        Self::FolderVideos,
        Self::Link,
    ];

    pub const fn definition(self) -> IconDefinition {
        use IconId::*;
        let (name, source) = match self {
            Menu => (
                "menu",
                IconSource::Svg("icons/fluent-system/navigation_16_regular.svg"),
            ),
            Add => (
                "add",
                IconSource::Svg("icons/fluent-system/add_16_regular.svg"),
            ),
            ZoomIn => (
                "zoom-in",
                IconSource::Svg("icons/fluent-system/zoom_in_16_regular.svg"),
            ),
            ZoomOut => (
                "zoom-out",
                IconSource::Svg("icons/fluent-system/zoom_out_16_regular.svg"),
            ),
            Close => (
                "close",
                IconSource::Svg("icons/fluent-system/dismiss_16_regular.svg"),
            ),
            Check => (
                "check",
                IconSource::Svg("icons/fluent-system/checkmark_16_regular.svg"),
            ),
            ChevronUp => (
                "chevron-up",
                IconSource::Svg("icons/fluent-system/chevron_up_16_regular.svg"),
            ),
            ChevronDown => (
                "chevron-down",
                IconSource::Svg("icons/fluent-system/chevron_down_16_regular.svg"),
            ),
            Back => (
                "back",
                IconSource::Svg("icons/fluent-system/arrow_left_16_regular.svg"),
            ),
            Forward => (
                "forward",
                IconSource::Svg("icons/fluent-system/arrow_right_16_regular.svg"),
            ),
            Refresh => (
                "refresh",
                IconSource::Svg("icons/fluent-system/arrow_clockwise_16_regular.svg"),
            ),
            Search => (
                "search",
                IconSource::Svg("icons/fluent-system/search_16_regular.svg"),
            ),
            Open => (
                "open",
                IconSource::Svg("icons/fluent-system/open_16_regular.svg"),
            ),
            Copy => (
                "copy",
                IconSource::Svg("icons/fluent-system/copy_16_regular.svg"),
            ),
            Cut => (
                "cut",
                IconSource::Svg("icons/fluent-system/cut_16_regular.svg"),
            ),
            Paste => (
                "paste",
                IconSource::Svg("icons/fluent-system/clipboard_paste_16_regular.svg"),
            ),
            Rename => (
                "rename",
                IconSource::Svg("icons/fluent-system/rename_16_regular.svg"),
            ),
            Delete => (
                "delete",
                IconSource::Svg("icons/fluent-system/delete_16_regular.svg"),
            ),
            CopyPath => (
                "copy-path",
                IconSource::Svg("icons/fluent-system/clipboard_link_16_regular.svg"),
            ),
            Properties => (
                "properties",
                IconSource::Svg("icons/fluent-system/info_16_regular.svg"),
            ),
            NewFolder => (
                "new-folder",
                IconSource::Svg("icons/fluent-system/folder_add_16_regular.svg"),
            ),
            NewFile => (
                "new-file",
                IconSource::Svg("icons/fluent-system/document_add_16_regular.svg"),
            ),
            CreateLink => (
                "create-link",
                IconSource::Svg("icons/fluent-system/link_add_16_regular.svg"),
            ),
            SplitPane => (
                "split-pane",
                IconSource::Svg("icons/fluent-system/split_vertical_16_regular.svg"),
            ),
            FolderTree => (
                "folder-tree",
                IconSource::Svg("icons/fluent-system/caret_right_16_filled.svg"),
            ),
            Expand => (
                "expand",
                IconSource::Svg("icons/fluent-system/chevron_right_16_regular.svg"),
            ),
            ReopenTab => (
                "reopen-tab",
                IconSource::Svg("icons/fluent-system/tab_desktop_arrow_clockwise_16_regular.svg"),
            ),
            WorkspaceHistory => (
                "workspace-history",
                IconSource::Svg("icons/fluent-system/history_16_regular.svg"),
            ),
            CloseOthers => (
                "close-others",
                IconSource::Svg("icons/fluent-system/dismiss_square_multiple_16_regular.svg"),
            ),
            Sort => (
                "sort",
                IconSource::Svg("icons/fluent-system/arrow_sort_16_regular.svg"),
            ),
            Calendar => (
                "calendar",
                IconSource::Svg("icons/fluent-system/calendar_16_regular.svg"),
            ),
            Rows => (
                "rows",
                IconSource::Svg("icons/fluent-system/list_16_regular.svg"),
            ),
            Tiles => (
                "tiles",
                IconSource::Svg("icons/fluent-system/grid_16_regular.svg"),
            ),
            // タブでは操作名に対応する ID で現在の固定状態を描く。
            Lock => (
                "pin",
                IconSource::Svg("icons/fluent-system/pin_off_16_regular.svg"),
            ),
            Unlock => (
                "unpin",
                IconSource::Svg("icons/fluent-system/pin_16_filled.svg"),
            ),
            Warning => (
                "warning",
                IconSource::Svg("icons/fluent-system/warning_16_regular.svg"),
            ),
            OperationCompleted => (
                "operation-completed",
                IconSource::Svg("icons/fluent-system/checkmark_circle_16_filled.svg"),
            ),
            OperationFailed => (
                "operation-failed",
                IconSource::Svg("icons/fluent-system/dismiss_circle_16_filled.svg"),
            ),
            OperationInProgress => (
                "operation-in-progress",
                IconSource::Svg("icons/fluent-system/arrow_sync_16_regular.svg"),
            ),
            OperationCancelled => (
                "operation-cancelled",
                IconSource::Svg("icons/fluent-system/subtract_circle_16_regular.svg"),
            ),
            Play => ("play", IconSource::TextGlyph('\u{25B6}')),
            FolderFallbackGlyph => (
                "folder-fallback-glyph",
                IconSource::Svg("icons/fluent-system/folder_16_regular.svg"),
            ),
            Libraries => (
                "libraries",
                IconSource::ColorSvg("icons/fluent11/places/folder-books.svg"),
            ),
            Network => (
                "network",
                IconSource::ColorSvg("icons/fluent11/places/network.svg"),
            ),
            RecycleBin => (
                "recycle-bin",
                IconSource::ColorSvg("icons/fluent11/places/trashcan-empty.svg"),
            ),
            WindowMinimize => (
                "window-minimize",
                IconSource::Svg("icons/window/minimize.svg"),
            ),
            WindowMaximize => (
                "window-maximize",
                IconSource::Svg("icons/window/maximize.svg"),
            ),
            WindowRestore => (
                "window-restore",
                IconSource::Svg("icons/window/restore.svg"),
            ),
            WindowClose => ("window-close", IconSource::Svg("icons/window/close.svg")),
            File => (
                "file",
                IconSource::ColorSvg("icons/fluent11/mimetypes/text-x-generic.svg"),
            ),
            FileText => (
                "file-text",
                IconSource::ColorSvg("icons/fluent11/mimetypes/text-plain.svg"),
            ),
            FilePng => (
                "file-png",
                IconSource::ColorSvg("icons/fluent11/mimetypes/image-png.svg"),
            ),
            FileJpeg => (
                "file-jpeg",
                IconSource::ColorSvg("icons/fluent11/mimetypes/image-jpeg.svg"),
            ),
            FileRust => (
                "file-rust",
                IconSource::ColorSvg("icons/fluent11/mimetypes/text-rust.svg"),
            ),
            FileJavaScript => (
                "file-javascript",
                IconSource::ColorSvg("icons/fluent11/mimetypes/application-javascript.svg"),
            ),
            FilePython => (
                "file-python",
                IconSource::ColorSvg("icons/fluent11/mimetypes/text-x-python.svg"),
            ),
            FileMarkdown => (
                "file-markdown",
                IconSource::ColorSvg("icons/fluent11/mimetypes/text-markdown.svg"),
            ),
            FileToml => (
                "file-toml",
                IconSource::ColorSvg("icons/fluent11/mimetypes/application-toml.svg"),
            ),
            FileJson => (
                "file-json",
                IconSource::ColorSvg("icons/fluent11/mimetypes/application-json.svg"),
            ),
            FileYaml => (
                "file-yaml",
                IconSource::ColorSvg("icons/fluent11/mimetypes/application-x-yaml.svg"),
            ),
            FolderBack => (
                "folder-back",
                IconSource::ColorSvg("icons/fluent11/folder-layers/back.svg"),
            ),
            FolderFront => (
                "folder-front",
                IconSource::ColorSvg("icons/fluent11/folder-layers/front.svg"),
            ),
            Folder => (
                "folder",
                IconSource::ColorSvg("icons/fluent11/places/folder.svg"),
            ),
            FolderDesktop => (
                "folder-desktop",
                IconSource::ColorSvg("icons/fluent11/places/folder-desktop.svg"),
            ),
            FolderDocuments => (
                "folder-documents",
                IconSource::ColorSvg("icons/fluent11/places/folder-documents.svg"),
            ),
            FolderDownloads => (
                "folder-downloads",
                IconSource::ColorSvg("icons/fluent11/places/folder-download.svg"),
            ),
            FolderHome => (
                "folder-home",
                IconSource::ColorSvg("icons/fluent11/places/folder-home.svg"),
            ),
            FolderMusic => (
                "folder-music",
                IconSource::ColorSvg("icons/fluent11/places/folder-music.svg"),
            ),
            FolderPictures => (
                "folder-pictures",
                IconSource::ColorSvg("icons/fluent11/places/folder-pictures.svg"),
            ),
            FolderVideos => (
                "folder-videos",
                IconSource::ColorSvg("icons/fluent11/places/folder-videos.svg"),
            ),
            Link => (
                "link",
                IconSource::Svg("icons/fluent-system/link_16_regular.svg"),
            ),
        };
        IconDefinition { name, source }
    }

    pub fn for_extension(extension: &str) -> Self {
        let extension = extension.trim_start_matches('.');
        if extension.eq_ignore_ascii_case("txt") {
            Self::FileText
        } else if extension.eq_ignore_ascii_case("png") {
            Self::FilePng
        } else if ["jpg", "jpeg", "jpe", "jfif"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
        {
            Self::FileJpeg
        } else if extension.eq_ignore_ascii_case("rs") {
            Self::FileRust
        } else if ["js", "mjs", "cjs"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
        {
            Self::FileJavaScript
        } else if ["py", "pyw"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
        {
            Self::FilePython
        } else if ["md", "markdown"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
        {
            Self::FileMarkdown
        } else if extension.eq_ignore_ascii_case("toml") {
            Self::FileToml
        } else if extension.eq_ignore_ascii_case("json") {
            Self::FileJson
        } else if ["yaml", "yml"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
        {
            Self::FileYaml
        } else {
            Self::File
        }
    }
}

pub struct ResourceAssets;

impl AssetSource for ResourceAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/fluent-system/add_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/add_16_regular.svg"
            )),
            "icons/fluent-system/zoom_in_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/zoom_in_16_regular.svg"
            )),
            "icons/fluent-system/zoom_out_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/zoom_out_16_regular.svg"
            )),
            "icons/fluent-system/arrow_clockwise_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/arrow_clockwise_16_regular.svg"
            )),
            "icons/fluent-system/arrow_left_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/arrow_left_16_regular.svg"
            )),
            "icons/fluent-system/arrow_right_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/arrow_right_16_regular.svg"
            )),
            "icons/fluent-system/arrow_sort_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/arrow_sort_16_regular.svg"
            )),
            "icons/fluent-system/calendar_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/calendar_16_regular.svg"
            )),
            "icons/fluent-system/checkmark_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/checkmark_16_regular.svg"
            )),
            "icons/fluent-system/chevron_down_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/chevron_down_16_regular.svg"
            )),
            "icons/fluent-system/chevron_right_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/chevron_right_16_regular.svg"
            )),
            "icons/fluent-system/chevron_up_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/chevron_up_16_regular.svg"
            )),
            "icons/fluent-system/clipboard_link_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/clipboard_link_16_regular.svg"
            )),
            "icons/fluent-system/clipboard_paste_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/clipboard_paste_16_regular.svg"
            )),
            "icons/fluent-system/copy_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/copy_16_regular.svg"
            )),
            "icons/fluent-system/cut_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/cut_16_regular.svg"
            )),
            "icons/fluent-system/delete_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/delete_16_regular.svg"
            )),
            "icons/fluent-system/dismiss_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/dismiss_16_regular.svg"
            )),
            "icons/fluent-system/dismiss_square_multiple_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/dismiss_square_multiple_16_regular.svg"
            )),
            "icons/fluent-system/document_add_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/document_add_16_regular.svg"
            )),
            "icons/fluent-system/folder_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/folder_16_regular.svg"
            )),
            "icons/fluent-system/folder_add_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/folder_add_16_regular.svg"
            )),
            "icons/fluent-system/grid_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/grid_16_regular.svg"
            )),
            "icons/fluent-system/info_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/info_16_regular.svg"
            )),
            "icons/fluent-system/link_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/link_16_regular.svg"
            )),
            "icons/fluent-system/link_add_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/link_add_16_regular.svg"
            )),
            "icons/fluent-system/list_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/list_16_regular.svg"
            )),
            "icons/fluent-system/caret_right_16_filled.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/caret_right_16_filled.svg"
            )),
            "icons/fluent-system/navigation_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/navigation_16_regular.svg"
            )),
            "icons/fluent-system/open_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/open_16_regular.svg"
            )),
            "icons/fluent-system/pin_16_filled.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/pin_16_filled.svg"
            )),
            "icons/fluent-system/pin_off_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/pin_off_16_regular.svg"
            )),
            "icons/fluent-system/rename_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/rename_16_regular.svg"
            )),
            "icons/fluent-system/search_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/search_16_regular.svg"
            )),
            "icons/fluent-system/split_vertical_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/split_vertical_16_regular.svg"
            )),
            "icons/fluent-system/history_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/history_16_regular.svg"
            )),
            "icons/fluent-system/tab_desktop_arrow_clockwise_16_regular.svg" => {
                Some(include_bytes!(
                    "../assets/icons/fluent-system/tab_desktop_arrow_clockwise_16_regular.svg"
                ))
            }
            "icons/fluent-system/warning_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/warning_16_regular.svg"
            )),
            "icons/fluent-system/checkmark_circle_16_filled.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/checkmark_circle_16_filled.svg"
            )),
            "icons/fluent-system/dismiss_circle_16_filled.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/dismiss_circle_16_filled.svg"
            )),
            "icons/fluent-system/arrow_sync_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/arrow_sync_16_regular.svg"
            )),
            "icons/fluent-system/subtract_circle_16_regular.svg" => Some(include_bytes!(
                "../assets/icons/fluent-system/subtract_circle_16_regular.svg"
            )),
            "icons/fluent11/folder-layers/back.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/folder-layers/back.svg"
            )),
            "icons/fluent11/folder-layers/front.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/folder-layers/front.svg"
            )),
            "icons/fluent11/mimetypes/text-plain.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/text-plain.svg"
            )),
            "icons/fluent11/mimetypes/image-png.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/image-png.svg"
            )),
            "icons/fluent11/mimetypes/image-jpeg.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/image-jpeg.svg"
            )),
            "icons/window/minimize.svg" => {
                Some(include_bytes!("../assets/icons/window/minimize.svg"))
            }
            "icons/window/maximize.svg" => {
                Some(include_bytes!("../assets/icons/window/maximize.svg"))
            }
            "icons/window/restore.svg" => {
                Some(include_bytes!("../assets/icons/window/restore.svg"))
            }
            "icons/window/close.svg" => Some(include_bytes!("../assets/icons/window/close.svg")),
            "icons/fallback/link.svg" => Some(include_bytes!("../assets/icons/fallback/link.svg")),
            "icons/fluent11/places/folder-books.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-books.svg"
            )),
            "icons/fluent11/places/network.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/network.svg"
            )),
            "icons/fluent11/places/trashcan-empty.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/trashcan-empty.svg"
            )),
            "icons/fluent11/mimetypes/text-x-generic.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/text-x-generic.svg"
            )),
            "icons/fluent11/mimetypes/text-rust.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/text-rust.svg"
            )),
            "icons/fluent11/mimetypes/application-javascript.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/application-javascript.svg"
            )),
            "icons/fluent11/mimetypes/text-x-python.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/text-x-python.svg"
            )),
            "icons/fluent11/mimetypes/text-markdown.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/text-markdown.svg"
            )),
            "icons/fluent11/mimetypes/application-toml.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/application-toml.svg"
            )),
            "icons/fluent11/mimetypes/application-json.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/application-json.svg"
            )),
            "icons/fluent11/mimetypes/application-x-yaml.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/mimetypes/application-x-yaml.svg"
            )),
            "icons/fluent11/places/folder.svg" => {
                Some(include_bytes!("../assets/icons/fluent11/places/folder.svg"))
            }
            "icons/fluent11/places/folder-desktop.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-desktop.svg"
            )),
            "icons/fluent11/places/folder-documents.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-documents.svg"
            )),
            "icons/fluent11/places/folder-download.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-download.svg"
            )),
            "icons/fluent11/places/folder-home.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-home.svg"
            )),
            "icons/fluent11/places/folder-music.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-music.svg"
            )),
            "icons/fluent11/places/folder-pictures.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-pictures.svg"
            )),
            "icons/fluent11/places/folder-videos.svg" => Some(include_bytes!(
                "../assets/icons/fluent11/places/folder-videos.svg"
            )),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let prefix = path.trim_end_matches('/');
        Ok(IconId::ALL
            .iter()
            .filter_map(|id| match id.definition().source {
                IconSource::Svg(asset) | IconSource::ColorSvg(asset)
                    if prefix.is_empty() || asset.starts_with(prefix) =>
                {
                    Some(SharedString::from(asset))
                }
                _ => None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn icon_names_are_unique() {
        let mut names = HashSet::new();
        for id in IconId::ALL {
            assert!(
                names.insert(id.definition().name),
                "duplicate icon name: {}",
                id.definition().name
            );
        }
    }

    #[test]
    fn every_svg_is_registered_and_nonempty() {
        let assets = ResourceAssets;
        for id in IconId::ALL {
            let (IconSource::Svg(path) | IconSource::ColorSvg(path)) = id.definition().source
            else {
                continue;
            };
            let bytes = assets
                .load(path)
                .unwrap()
                .unwrap_or_else(|| panic!("unregistered SVG: {path}"));
            assert!(!bytes.is_empty(), "empty SVG: {path}");
        }
    }

    #[test]
    fn extension_lookup_is_case_insensitive_and_has_a_fallback() {
        assert_eq!(IconId::for_extension(".PNG"), IconId::FilePng);
        assert_eq!(IconId::for_extension("jpg"), IconId::FileJpeg);
        assert_eq!(IconId::for_extension(".JPEG"), IconId::FileJpeg);
        assert_eq!(IconId::for_extension("txt"), IconId::FileText);
        assert_eq!(IconId::for_extension(".RS"), IconId::FileRust);
        assert_eq!(IconId::for_extension("yMl"), IconId::FileYaml);
        assert_eq!(IconId::for_extension("tsx"), IconId::File);
        assert_eq!(IconId::for_extension(""), IconId::File);
    }

    #[test]
    fn color_svgs_rasterize_at_display_resolution_and_keep_folder_color() {
        use gpui::{DevicePixels, SvgRenderer, SvgSize, size};
        let renderer = SvgRenderer::new(std::sync::Arc::new(ResourceAssets));
        for id in IconId::ALL {
            let IconSource::ColorSvg(path) = id.definition().source else {
                continue;
            };
            let bytes = ResourceAssets.load(path).unwrap().unwrap();
            let parsed = renderer.parse_svg(&bytes).unwrap();
            for pixels in [40, 160] {
                let image = renderer
                    .render_parsed(
                        &parsed,
                        SvgSize::Size(size(DevicePixels(pixels), DevicePixels(pixels))),
                    )
                    .unwrap();
                assert_eq!(
                    image.size(0),
                    size(DevicePixels(pixels), DevicePixels(pixels))
                );
                assert!(
                    image
                        .as_bytes(0)
                        .unwrap()
                        .chunks_exact(4)
                        .any(|pixel| pixel[3] > 0),
                    "empty image: {path}"
                );
                if *id == IconId::Folder {
                    // RenderImageはBGRA。黄色の元絵が単色マスクに変わっていないことを確認する。
                    assert!(
                        image
                            .as_bytes(0)
                            .unwrap()
                            .chunks_exact(4)
                            .any(|p| p[3] > 200 && p[2] > 180 && p[1] > 100 && p[0] < 100)
                    );
                }
            }
        }
    }
}
