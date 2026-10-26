//! セッションの保存形式と、書く順序の契約。
//!
//! ここは「何を保存するか」「何を拒否するか」「書く順序をどう保証するか」を知る。
//! いつ書くかと、生きているタブからの写し取りは `src/persistence.rs`。
//! GPUI も `FilerShell` も知らないので、判定は単体テストで固定できる。
//!
//! 保存しないもの: GPUI の Entity / FocusHandle / subscription、process-local な
//! `ViewId` / `ScanId` / `WatchToken`、列挙済み entries、thumbnail、翻訳済み名称、
//! row height、hover、operation job の再実行指示。閉じたタブ用に navigation values と
//! その bookmark は保存するが、runtime object や列挙結果は持ち込まない。
//!
//! 保存する path は lossless に運ぶ。`to_string_lossy` を通すと孤立 surrogate が
//! 置換文字になり、保存したつもりの path が復元できない。

pub(crate) mod windows;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// この版が書く schema。これより新しい schema は読むだけで上書きしない。
pub const SCHEMA_VERSION: u32 = 11;

/// parse の前後で見る上限。超えたデータを黙って切り捨てない。
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TABS: usize = 256;
pub const MAX_PANES: usize = 32;
pub const MAX_WORKSPACES: usize = 32;
pub const MAX_WINDOWS: usize = 32;
pub const MAX_WORKSPACE_NAME_BYTES: usize = 128;
const MAX_PANE_LAYOUT_DEPTH: usize = 64;
pub const MAX_HISTORY_ENTRIES: usize = 100;
pub const MAX_CLOSED_TABS: usize = 100;
pub const MAX_CLOSED_WORKSPACES: usize = 100;
pub const MAX_VISIBLE_CLOSED_WORKSPACES: usize = 12;
pub const MAX_CLOSED_WORKSPACE_HISTORY_BYTES: usize = MAX_SNAPSHOT_BYTES / 2;
const MAX_ARCHIVE_COMPONENTS: usize = 128;
pub const MAX_CLOSED_BOOKMARK_SELECTION_KEYS: usize = 4096;
/// Closed history uses at most half the file budget, leaving room for live tabs and window state.
pub const MAX_CLOSED_HISTORY_BYTES: usize = MAX_SNAPSHOT_BYTES / 2;
pub const MAX_PATH_UNITS: usize = 32_768;
const MAX_ENTRY_IDENTITY_BYTES: usize = 32_768;

/// 保存の debounce。変更が静まってから書く。
pub(crate) const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// 最初の未保存変更から数えた上限。連続する変更で永久に書けない状態を作らない。
pub(crate) const SAVE_DEBOUNCE_CAP: Duration = Duration::from_millis(2000);

/// 保存を書くべきか。
///
/// 変更が静まって [`SAVE_DEBOUNCE`]、または最初の未保存変更から
/// [`SAVE_DEBOUNCE_CAP`]。上限が無いと、変更が続く間は一度も書けない。
pub(crate) fn should_write(quiet: Duration, pending: Duration) -> bool {
    quiet >= SAVE_DEBOUNCE || pending >= SAVE_DEBOUNCE_CAP
}

/// 起動時の復元の判断。
#[derive(Debug, PartialEq)]
pub(crate) enum RestoreOutcome {
    /// 保存が無い。既定で始める。
    Absent,
    /// 復元できる。
    Restored(Box<SessionSnapshot>),
    /// 読めない。primary は保持し、defaults で上書きしない。
    Unreadable(String),
    /// この版より新しい。読むだけで上書きしない。
    FutureSchema { found: u32 },
}

/// primary と backup の bytes から復元の判断をする。
///
/// 判定そのものを純粋にして、filesystem は呼び出し側に任せる。
/// - primary が無ければ `Absent`。backup だけから復元しない。
/// - future schema は読むだけで上書きしない。古い backup へも降りない
///   （降りると、新しい版の保存を古い内容で置換してしまう）。
/// - 壊れた primary は保持し、valid な backup があればそれを使う。
pub(crate) fn resolve_restore(primary: Option<&[u8]>, backup: Option<&[u8]>) -> RestoreOutcome {
    let Some(primary) = primary else {
        return RestoreOutcome::Absent;
    };
    match SessionSnapshot::from_json(primary) {
        Ok(snapshot) => RestoreOutcome::Restored(Box::new(snapshot)),
        Err(SessionError::FutureSchema { found, .. }) => RestoreOutcome::FutureSchema { found },
        Err(primary_error) => match backup.map(SessionSnapshot::from_json) {
            Some(Ok(snapshot)) => RestoreOutcome::Restored(Box::new(snapshot)),
            _ => RestoreOutcome::Unreadable(primary_error.to_string()),
        },
    }
}

/// 保存できない理由。呼び出し側が可視の診断に使う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// JSON にならない、構造が違う。
    Malformed(String),
    /// この版より新しい schema。読むだけで上書きしない。
    FutureSchema { found: u32, supported: u32 },
    /// 上限を超えた。切り捨てて「正常保存」にしない。
    LimitExceeded(&'static str),
    /// snapshot 内で tab key が重複している。
    DuplicateTabKey(u64),
    /// path として不正（空、NUL、相対、長すぎる）。
    InvalidPath(String),
    /// この OS では復元できない。別 OS の path をそのまま解釈しない。
    #[allow(dead_code)] // Windows 以外の実装でだけ作られる。
    UnsupportedPlatform,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "malformed session: {reason}"),
            Self::FutureSchema { found, supported } => {
                write!(f, "session schema {found} is newer than {supported}")
            }
            Self::LimitExceeded(what) => write!(f, "session exceeds the {what} limit"),
            Self::DuplicateTabKey(key) => write!(f, "duplicate tab key {key}"),
            Self::InvalidPath(reason) => write!(f, "invalid path: {reason}"),
            Self::UnsupportedPlatform => write!(f, "unsupported platform"),
        }
    }
}

/// 保存する path。OS の表現をそのまま持つ。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding")]
pub enum PathValue {
    /// Windows の `OsString` を UTF-16 code unit のまま。
    #[serde(rename = "windows-utf16")]
    WindowsUtf16 { units: Vec<u16> },
}

/// 保存する location。path を持たない仮想の場所を偽の path で表さない。
///
/// v2 からは kind を必須にする。path の byte/code unit 表現は `PathValue` のままなので、
/// v1 の lossless UTF-16 契約は変えない。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocationValue {
    Path {
        path: PathValue,
    },
    Archive {
        source: PathValue,
        components: Vec<String>,
    },
    ThisPc,
    Libraries,
    Network,
    RecycleBin,
    ShellItem {
        root: ShellRootValue,
        parsing_name: String,
        display_name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellRootValue {
    Libraries,
    Network,
    RecycleBin,
}

impl LocationValue {
    pub fn from_location(location: &crate::location::Location) -> Result<Self, SessionError> {
        match location {
            crate::location::Location::Path(path) => Ok(Self::Path {
                path: PathValue::from_path(path)?,
            }),
            crate::location::Location::Archive { source, components } => {
                let value = Self::Archive {
                    source: PathValue::from_path(source)?,
                    components: components.clone(),
                };
                value.validate()?;
                Ok(value)
            }
            crate::location::Location::ThisPc => Ok(Self::ThisPc),
            crate::location::Location::ShellRoot(crate::location::ShellRoot::Libraries) => {
                Ok(Self::Libraries)
            }
            crate::location::Location::ShellRoot(crate::location::ShellRoot::Network) => {
                Ok(Self::Network)
            }
            crate::location::Location::ShellRoot(crate::location::ShellRoot::RecycleBin) => {
                Ok(Self::RecycleBin)
            }
            crate::location::Location::ShellItem {
                root,
                parsing_name,
                display_name,
            } => {
                validate_nonempty_bounded_text(parsing_name, "shell parsing name")?;
                validate_nonempty_bounded_text(display_name, "shell display name")?;
                let value = Self::ShellItem {
                    root: (*root).into(),
                    parsing_name: parsing_name.clone(),
                    display_name: display_name.clone(),
                };
                value.validate()?;
                Ok(value)
            }
        }
    }

    pub fn to_location(&self) -> Result<crate::location::Location, SessionError> {
        match self {
            Self::Path { path } => Ok(crate::location::Location::Path(path.to_path_buf()?)),
            Self::Archive { source, components } => {
                self.validate()?;
                Ok(crate::location::Location::Archive {
                    source: source.to_path_buf()?,
                    components: components.clone(),
                })
            }
            Self::ThisPc => Ok(crate::location::Location::ThisPc),
            Self::Libraries => Ok(crate::location::Location::ShellRoot(
                crate::location::ShellRoot::Libraries,
            )),
            Self::Network => Ok(crate::location::Location::ShellRoot(
                crate::location::ShellRoot::Network,
            )),
            Self::RecycleBin => Ok(crate::location::Location::ShellRoot(
                crate::location::ShellRoot::RecycleBin,
            )),
            Self::ShellItem {
                root,
                parsing_name,
                display_name,
            } => {
                self.validate()?;
                Ok(crate::location::Location::ShellItem {
                    root: (*root).into(),
                    parsing_name: parsing_name.clone(),
                    display_name: display_name.clone(),
                })
            }
        }
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        match self {
            Self::Path { path } => path.validate(),
            Self::Archive { source, components } => {
                source.validate()?;
                validate_archive_components(components)
            }
            Self::ThisPc => Ok(()),
            Self::Libraries => Ok(()),
            Self::Network => Ok(()),
            Self::RecycleBin => Ok(()),
            Self::ShellItem {
                parsing_name,
                display_name,
                ..
            } => {
                validate_nonempty_bounded_text(parsing_name, "shell parsing name")?;
                validate_nonempty_bounded_text(display_name, "shell display name")
            }
        }
    }
}

impl From<crate::location::ShellRoot> for ShellRootValue {
    fn from(root: crate::location::ShellRoot) -> Self {
        match root {
            crate::location::ShellRoot::Libraries => Self::Libraries,
            crate::location::ShellRoot::Network => Self::Network,
            crate::location::ShellRoot::RecycleBin => Self::RecycleBin,
        }
    }
}

impl From<ShellRootValue> for crate::location::ShellRoot {
    fn from(root: ShellRootValue) -> Self {
        match root {
            ShellRootValue::Libraries => Self::Libraries,
            ShellRootValue::Network => Self::Network,
            ShellRootValue::RecycleBin => Self::RecycleBin,
        }
    }
}

fn validate_nonempty_bounded_text(value: &str, what: &'static str) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!("empty {what}")));
    }
    if value.len() > MAX_ENTRY_IDENTITY_BYTES {
        return Err(SessionError::LimitExceeded(what));
    }
    Ok(())
}

impl PathValue {
    /// `Path` を保存用の値にする。
    ///
    /// filesystem の存在は要求しない。offline の drive / UNC の tab も残す。
    pub fn from_path(path: &Path) -> Result<Self, SessionError> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            let value = Self::WindowsUtf16 {
                units: path.as_os_str().encode_wide().collect(),
            };
            value.validate()?;
            Ok(value)
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(SessionError::UnsupportedPlatform)
        }
    }

    pub fn to_path_buf(&self) -> Result<PathBuf, SessionError> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            let Self::WindowsUtf16 { units } = self;
            Ok(std::ffi::OsString::from_wide(units).into())
        }
        #[cfg(not(windows))]
        {
            Err(SessionError::UnsupportedPlatform)
        }
    }

    /// 保存できる形か。
    ///
    /// NUL と相対 path を拒否し、長さの上限を見る。drive 絶対 (`C:\`) か
    /// UNC (`\\server\share`) だけを許す。
    pub fn validate(&self) -> Result<(), SessionError> {
        let Self::WindowsUtf16 { units } = self;
        if units.is_empty() {
            return Err(SessionError::InvalidPath("empty".into()));
        }
        if units.len() > MAX_PATH_UNITS {
            return Err(SessionError::LimitExceeded("path length"));
        }
        if units.contains(&0) {
            return Err(SessionError::InvalidPath("NUL".into()));
        }
        let backslash = b'\\' as u16;
        let slash = b'/' as u16;
        let is_separator = |unit: u16| unit == backslash || unit == slash;
        let letter = |unit: u16| {
            (b'A' as u16..=b'Z' as u16).contains(&unit)
                || (b'a' as u16..=b'z' as u16).contains(&unit)
        };
        // `C:\` か `C:/`
        let drive_absolute = units.len() >= 3
            && letter(units[0])
            && units[1] == b':' as u16
            && is_separator(units[2]);
        // `\\server\share`
        let unc = units.len() >= 5 && is_separator(units[0]) && is_separator(units[1]);
        if drive_absolute || unc {
            Ok(())
        } else {
            Err(SessionError::InvalidPath("relative".into()))
        }
    }
}

/// 表示形式。v5 の `Layout` と同じ3種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutValue {
    Rows,
    Tiles,
    Icons,
}

/// 並べ替えの列。v5 の `ColumnKind` と同じ4種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColumnValue {
    Name,
    Kind,
    Size,
    Modified,
}

/// 並べ替えの状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "column")]
pub enum SortValue {
    /// 既定。フォルダ優先 + 名前順。
    None,
    Asc(ColumnValue),
    Desc(ColumnValue),
}

/// Selection bookmark key encoded independently from runtime `EntryKey`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EntryKeyValue {
    Path {
        path: PathValue,
        entry_kind: EntryKindValue,
    },
    Archive {
        source: PathValue,
        components: Vec<String>,
        entry_kind: EntryKindValue,
    },
    Demo {
        id: u64,
    },
    Virtual {
        identity: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKindValue {
    Folder,
    File,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FocusOriginValue {
    #[default]
    SelectionIntent,
    NavigationOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionSnapshotValue {
    pub selected: Vec<EntryKeyValue>,
    pub anchor: Option<EntryKeyValue>,
    pub focus: Option<EntryKeyValue>,
    #[serde(default)]
    pub focus_origin: FocusOriginValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewBookmarkValue {
    pub key: EntryKeyValue,
    pub offset: f32,
    pub selection: SelectionSnapshotValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavigationEntryValue {
    pub location: LocationValue,
    pub bookmark: Option<ViewBookmarkValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavigationHistoryValue {
    pub entries: Vec<NavigationEntryValue>,
    pub cursor: usize,
}

/// One closed tab's restorable values. It intentionally has no runtime identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClosedTabSnapshot {
    pub tab: TabSnapshot,
    pub original_index: usize,
    pub was_active: bool,
    pub navigation: NavigationHistoryValue,
}

impl ClosedTabSnapshot {
    /// Estimate a live closed record without allocating DTO vectors, for aggregate preflight.
    pub fn estimate_encoded_size(
        tab: &TabSnapshot,
        _original_index: usize,
        _was_active: bool,
        navigation: &crate::navigation_history::NavigationHistorySnapshot,
    ) -> Result<usize, SessionError> {
        validate_tab(tab)?;
        preflight_navigation_snapshot(
            navigation,
            estimate_location_value_size(&tab.location)?.saturating_add(768),
        )
    }

    pub fn from_parts(
        tab: TabSnapshot,
        original_index: usize,
        was_active: bool,
        navigation: &crate::navigation_history::NavigationHistorySnapshot,
    ) -> Result<Self, SessionError> {
        Self::estimate_encoded_size(&tab, original_index, was_active, navigation)?;
        let snapshot = Self {
            tab,
            original_index,
            was_active,
            navigation: NavigationHistoryValue::try_from(navigation)?,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn navigation_snapshot(
        &self,
    ) -> Result<crate::navigation_history::NavigationHistorySnapshot, SessionError> {
        self.validate()?;
        self.navigation.to_navigation_snapshot()
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        validate_tab(&self.tab)?;
        let navigation = &self.navigation;
        navigation.validate()?;
        if self.tab.location != navigation.entries[navigation.cursor].location {
            return Err(SessionError::Malformed(
                "closed tab current location mismatch".into(),
            ));
        }
        Ok(())
    }

    fn estimated_stored_size(&self) -> Result<usize, SessionError> {
        self.validate()?;
        Ok(estimate_navigation_value_size(&self.navigation)?
            .saturating_add(estimate_location_value_size(&self.tab.location)?)
            .saturating_add(768))
    }
}

impl EntryKeyValue {
    pub fn from_entry_key(key: &crate::entry_identity::EntryKey) -> Result<Self, SessionError> {
        use crate::entry_identity::EntryKey;
        match key {
            EntryKey::Path { path, kind } => Ok(Self::Path {
                path: PathValue::from_path(path)?,
                entry_kind: match kind {
                    crate::directory::EntryKind::Folder => EntryKindValue::Folder,
                    crate::directory::EntryKind::File => EntryKindValue::File,
                },
            }),
            EntryKey::Archive {
                source,
                components,
                kind,
            } => {
                validate_archive_components(components)?;
                Ok(Self::Archive {
                    source: PathValue::from_path(source)?,
                    components: components.clone(),
                    entry_kind: match kind {
                        crate::directory::EntryKind::Folder => EntryKindValue::Folder,
                        crate::directory::EntryKind::File => EntryKindValue::File,
                    },
                })
            }
            EntryKey::Demo(id) => Ok(Self::Demo { id: *id }),
            EntryKey::Virtual(identity)
                if !identity.is_empty() && identity.len() <= MAX_ENTRY_IDENTITY_BYTES =>
            {
                Ok(Self::Virtual {
                    identity: identity.clone(),
                })
            }
            EntryKey::Virtual(_) => Err(SessionError::LimitExceeded("entry identity")),
        }
    }

    pub fn to_entry_key(&self) -> Result<crate::entry_identity::EntryKey, SessionError> {
        use crate::entry_identity::EntryKey;
        match self {
            Self::Path { path, entry_kind } => {
                path.validate()?;
                Ok(EntryKey::Path {
                    path: path.to_path_buf()?,
                    kind: match entry_kind {
                        EntryKindValue::Folder => crate::directory::EntryKind::Folder,
                        EntryKindValue::File => crate::directory::EntryKind::File,
                    },
                })
            }
            Self::Archive {
                source,
                components,
                entry_kind,
            } => {
                source.validate()?;
                validate_archive_components(components)?;
                Ok(EntryKey::Archive {
                    source: source.to_path_buf()?,
                    components: components.clone(),
                    kind: match entry_kind {
                        EntryKindValue::Folder => crate::directory::EntryKind::Folder,
                        EntryKindValue::File => crate::directory::EntryKind::File,
                    },
                })
            }
            Self::Demo { id } => Ok(EntryKey::Demo(*id)),
            Self::Virtual { identity }
                if !identity.is_empty() && identity.len() <= MAX_ENTRY_IDENTITY_BYTES =>
            {
                Ok(EntryKey::Virtual(identity.clone()))
            }
            Self::Virtual { .. } => Err(SessionError::LimitExceeded("entry identity")),
        }
    }

    fn validate(&self) -> Result<(), SessionError> {
        match self {
            Self::Path { path, .. } => path.validate(),
            Self::Archive {
                source, components, ..
            } => {
                source.validate()?;
                validate_archive_components(components)
            }
            Self::Demo { .. } => Ok(()),
            Self::Virtual { identity }
                if !identity.is_empty() && identity.len() <= MAX_ENTRY_IDENTITY_BYTES =>
            {
                Ok(())
            }
            Self::Virtual { .. } => Err(SessionError::LimitExceeded("entry identity")),
        }
    }
}

impl SelectionSnapshotValue {
    pub fn from_selection(
        snapshot: &crate::selection::SelectionSnapshot,
    ) -> Result<Self, SessionError> {
        let (selected, anchor, focus, origin) = snapshot.parts();
        Ok(Self {
            selected: selected
                .iter()
                .map(EntryKeyValue::from_entry_key)
                .collect::<Result<_, _>>()?,
            anchor: anchor.map(EntryKeyValue::from_entry_key).transpose()?,
            focus: focus.map(EntryKeyValue::from_entry_key).transpose()?,
            focus_origin: match origin {
                crate::selection::FocusOrigin::SelectionIntent => FocusOriginValue::SelectionIntent,
                crate::selection::FocusOrigin::NavigationOnly => FocusOriginValue::NavigationOnly,
            },
        })
    }

    pub fn to_selection(&self) -> Result<crate::selection::SelectionSnapshot, SessionError> {
        Ok(crate::selection::SelectionSnapshot::from_parts(
            self.selected
                .iter()
                .map(EntryKeyValue::to_entry_key)
                .collect::<Result<_, _>>()?,
            self.anchor
                .as_ref()
                .map(EntryKeyValue::to_entry_key)
                .transpose()?,
            self.focus
                .as_ref()
                .map(EntryKeyValue::to_entry_key)
                .transpose()?,
            match self.focus_origin {
                FocusOriginValue::SelectionIntent => crate::selection::FocusOrigin::SelectionIntent,
                FocusOriginValue::NavigationOnly => crate::selection::FocusOrigin::NavigationOnly,
            },
        ))
    }

    fn validate(&self) -> Result<(), SessionError> {
        for key in &self.selected {
            key.validate()?;
        }
        if let Some(key) = &self.anchor {
            key.validate()?;
        }
        if let Some(key) = &self.focus {
            key.validate()?;
        }
        Ok(())
    }
}

impl TryFrom<&crate::navigation_history::NavigationHistorySnapshot> for NavigationHistoryValue {
    type Error = SessionError;
    fn try_from(
        snapshot: &crate::navigation_history::NavigationHistorySnapshot,
    ) -> Result<Self, Self::Error> {
        // Limit checks happen before any `EntryKeyValue` or selection vectors are allocated.
        preflight_navigation_snapshot(snapshot, 0)?;
        let entries = snapshot
            .entries
            .iter()
            .map(|entry| {
                Ok(NavigationEntryValue {
                    location: LocationValue::from_location(&entry.location)?,
                    bookmark: entry
                        .bookmark
                        .as_ref()
                        .map(|bookmark| {
                            Ok(ViewBookmarkValue {
                                key: EntryKeyValue::from_entry_key(&bookmark.key)?,
                                offset: bookmark.offset,
                                selection: SelectionSnapshotValue::from_selection(
                                    &bookmark.selection,
                                )?,
                            })
                        })
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        let value = Self {
            entries,
            cursor: snapshot.cursor,
        };
        value.validate()?;
        Ok(value)
    }
}

fn preflight_navigation_snapshot(
    snapshot: &crate::navigation_history::NavigationHistorySnapshot,
    record_overhead: usize,
) -> Result<usize, SessionError> {
    if snapshot.entries.is_empty() || snapshot.entries.len() > MAX_HISTORY_ENTRIES {
        return Err(SessionError::LimitExceeded("closed tab navigation entries"));
    }
    if snapshot.cursor >= snapshot.entries.len() {
        return Err(SessionError::Malformed(
            "closed tab navigation cursor".into(),
        ));
    }

    let mut selected_count = 0usize;
    for entry in &snapshot.entries {
        if let Some(bookmark) = &entry.bookmark {
            selected_count = selected_count.saturating_add(bookmark.selection.parts().0.len());
            if selected_count > MAX_CLOSED_BOOKMARK_SELECTION_KEYS {
                return Err(SessionError::LimitExceeded(
                    "closed tab bookmark selection keys",
                ));
            }
        }
    }

    let mut estimated = 512usize.saturating_add(record_overhead);
    for entry in &snapshot.entries {
        estimated = estimated
            .saturating_add(96)
            .saturating_add(estimate_location_size(&entry.location)?);
        if let Some(bookmark) = &entry.bookmark {
            if !bookmark.offset.is_finite() || bookmark.offset < 0.0 {
                return Err(SessionError::Malformed("bookmark offset".into()));
            }
            estimated = estimated
                .saturating_add(256)
                .saturating_add(estimate_entry_key_size(&bookmark.key)?);
            let (selected, anchor, focus, _) = bookmark.selection.parts();
            for key in selected {
                estimated = estimated.saturating_add(estimate_entry_key_size(key)?);
            }
            if let Some(key) = anchor {
                estimated = estimated.saturating_add(estimate_entry_key_size(key)?);
            }
            if let Some(key) = focus {
                estimated = estimated.saturating_add(estimate_entry_key_size(key)?);
            }
        }
        if estimated > MAX_SNAPSHOT_BYTES {
            return Err(SessionError::LimitExceeded("closed tab encoded size"));
        }
    }
    Ok(estimated)
}

/// Close-time borrowed preflight. This deliberately runs before history/selection cloning.
pub(crate) fn preflight_closed_tab_capture(
    location: &crate::location::Location,
    history: &crate::navigation_history::NavigationHistory,
    replace_current_bookmark: bool,
    current_selection_count: usize,
    current_selection_bytes: usize,
) -> Result<(), SessionError> {
    let entries = history.entries_ref();
    if entries.is_empty() || entries.len() > MAX_HISTORY_ENTRIES {
        return Err(SessionError::LimitExceeded("closed tab navigation entries"));
    }
    let mut selected_count = current_selection_count;
    let mut estimated = 512usize
        .saturating_add(estimate_location_size(location)?)
        .saturating_add(768)
        .saturating_add(current_selection_bytes)
        .saturating_add(512);
    for (index, entry) in entries.iter().enumerate() {
        estimated = estimated
            .saturating_add(96)
            .saturating_add(estimate_location_size(&entry.location)?);
        if let Some(bookmark) = &entry.bookmark
            && !(replace_current_bookmark && index == history.cursor())
        {
            let (selected, anchor, focus, _) = bookmark.selection.parts();
            selected_count = selected_count.saturating_add(selected.len());
            estimated = estimated
                .saturating_add(256)
                .saturating_add(estimate_entry_key_size(&bookmark.key)?);
            for key in selected {
                estimated = estimated.saturating_add(estimate_entry_key_size(key)?);
            }
            if let Some(key) = anchor {
                estimated = estimated.saturating_add(estimate_entry_key_size(key)?);
            }
            if let Some(key) = focus {
                estimated = estimated.saturating_add(estimate_entry_key_size(key)?);
            }
        }
    }
    if selected_count > MAX_CLOSED_BOOKMARK_SELECTION_KEYS {
        return Err(SessionError::LimitExceeded(
            "closed tab bookmark selection keys",
        ));
    }
    if estimated > MAX_CLOSED_HISTORY_BYTES {
        return Err(SessionError::LimitExceeded("closed tab encoded size"));
    }
    Ok(())
}

fn estimate_location_size(location: &crate::location::Location) -> Result<usize, SessionError> {
    match location {
        crate::location::Location::Path(path) => estimate_path_value_size(path),
        crate::location::Location::Archive { source, components } => {
            validate_archive_components(components)?;
            let mut size = estimate_path_value_size(source)?.saturating_add(64);
            for component in components {
                size = size.saturating_add(estimate_json_string_size(component));
            }
            Ok(size)
        }
        crate::location::Location::ThisPc | crate::location::Location::ShellRoot(_) => Ok(32),
        crate::location::Location::ShellItem {
            parsing_name,
            display_name,
            ..
        } => {
            validate_nonempty_bounded_text(parsing_name, "shell parsing name")?;
            validate_nonempty_bounded_text(display_name, "shell display name")?;
            Ok(64usize
                .saturating_add(estimate_json_string_size(parsing_name))
                .saturating_add(estimate_json_string_size(display_name)))
        }
    }
}

pub(crate) fn estimate_entry_key_size(
    key: &crate::entry_identity::EntryKey,
) -> Result<usize, SessionError> {
    match key {
        crate::entry_identity::EntryKey::Path { path, .. } => {
            Ok(estimate_path_value_size(path)?.saturating_add(96))
        }
        crate::entry_identity::EntryKey::Archive {
            source, components, ..
        } => {
            validate_archive_components(components)?;
            Ok(estimate_path_value_size(source)?
                .saturating_add(96)
                .saturating_add(
                    components
                        .iter()
                        .map(|part| estimate_json_string_size(part))
                        .sum::<usize>(),
                ))
        }
        crate::entry_identity::EntryKey::Demo(_) => Ok(64),
        crate::entry_identity::EntryKey::Virtual(identity) => {
            validate_nonempty_bounded_text(identity, "entry identity")?;
            Ok(64usize.saturating_add(estimate_json_string_size(identity)))
        }
    }
}

fn estimate_json_string_size(value: &str) -> usize {
    value.chars().fold(2usize, |size, character| {
        size.saturating_add(
            if character == '"' || character == '\\' || character <= '\u{1f}' {
                6
            } else {
                character.len_utf8()
            },
        )
    })
}

fn estimate_path_value_size(path: &Path) -> Result<usize, SessionError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        estimate_path_units_size(path.as_os_str().encode_wide())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(SessionError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
fn estimate_path_units_size(units: impl Iterator<Item = u16>) -> Result<usize, SessionError> {
    let mut count = 0usize;
    let mut encoded = 96usize; // tagged value and array punctuation
    for unit in units {
        count = count.saturating_add(1);
        encoded = encoded.saturating_add(decimal_digits(unit) + 1);
    }
    if count == 0 {
        return Err(SessionError::InvalidPath("empty".into()));
    }
    if count > MAX_PATH_UNITS {
        return Err(SessionError::LimitExceeded("path length"));
    }
    Ok(encoded)
}

#[cfg(windows)]
fn decimal_digits(mut value: u16) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn estimate_location_value_size(location: &LocationValue) -> Result<usize, SessionError> {
    location.validate()?;
    match location {
        LocationValue::Path {
            path: PathValue::WindowsUtf16 { units },
        } => {
            #[cfg(windows)]
            {
                estimate_path_units_size(units.iter().copied())
            }
            #[cfg(not(windows))]
            {
                let _ = units;
                Err(SessionError::UnsupportedPlatform)
            }
        }
        LocationValue::Archive {
            source: PathValue::WindowsUtf16 { units },
            components,
        } => {
            validate_archive_components(components)?;
            #[cfg(windows)]
            {
                Ok(estimate_path_units_size(units.iter().copied())?
                    .saturating_add(64)
                    .saturating_add(
                        components
                            .iter()
                            .map(|part| estimate_json_string_size(part))
                            .sum::<usize>(),
                    ))
            }
            #[cfg(not(windows))]
            {
                let _ = units;
                Err(SessionError::UnsupportedPlatform)
            }
        }
        LocationValue::ShellItem {
            parsing_name,
            display_name,
            ..
        } => Ok(64usize
            .saturating_add(estimate_json_string_size(parsing_name))
            .saturating_add(estimate_json_string_size(display_name))),
        _ => Ok(32),
    }
}

fn estimate_stored_entry_key_size(key: &EntryKeyValue) -> Result<usize, SessionError> {
    match key {
        EntryKeyValue::Path {
            path: PathValue::WindowsUtf16 { units },
            ..
        } => {
            #[cfg(windows)]
            {
                Ok(estimate_path_units_size(units.iter().copied())?.saturating_add(96))
            }
            #[cfg(not(windows))]
            {
                let _ = units;
                Err(SessionError::UnsupportedPlatform)
            }
        }
        EntryKeyValue::Archive {
            source: PathValue::WindowsUtf16 { units },
            components,
            ..
        } => {
            validate_archive_components(components)?;
            #[cfg(windows)]
            {
                Ok(estimate_path_units_size(units.iter().copied())?
                    .saturating_add(96)
                    .saturating_add(
                        components
                            .iter()
                            .map(|part| estimate_json_string_size(part))
                            .sum::<usize>(),
                    ))
            }
            #[cfg(not(windows))]
            {
                let _ = units;
                Err(SessionError::UnsupportedPlatform)
            }
        }
        EntryKeyValue::Demo { .. } => Ok(64),
        EntryKeyValue::Virtual { identity } => {
            Ok(64usize.saturating_add(estimate_json_string_size(identity)))
        }
    }
}

fn estimate_navigation_value_size(
    navigation: &NavigationHistoryValue,
) -> Result<usize, SessionError> {
    navigation.validate()?;
    let mut estimated = 512usize;
    for entry in &navigation.entries {
        estimated = estimated
            .saturating_add(96)
            .saturating_add(estimate_location_value_size(&entry.location)?);
        if let Some(bookmark) = &entry.bookmark {
            estimated = estimated
                .saturating_add(256)
                .saturating_add(estimate_stored_entry_key_size(&bookmark.key)?);
            for key in &bookmark.selection.selected {
                estimated = estimated.saturating_add(estimate_stored_entry_key_size(key)?);
            }
            if let Some(key) = &bookmark.selection.anchor {
                estimated = estimated.saturating_add(estimate_stored_entry_key_size(key)?);
            }
            if let Some(key) = &bookmark.selection.focus {
                estimated = estimated.saturating_add(estimate_stored_entry_key_size(key)?);
            }
        }
        if estimated > MAX_SNAPSHOT_BYTES {
            return Err(SessionError::LimitExceeded("closed tab encoded size"));
        }
    }
    Ok(estimated)
}

impl NavigationHistoryValue {
    pub fn to_navigation_snapshot(
        &self,
    ) -> Result<crate::navigation_history::NavigationHistorySnapshot, SessionError> {
        self.validate()?;
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                Ok(crate::navigation_history::HistoryEntry {
                    location: entry.location.to_location()?,
                    bookmark: entry
                        .bookmark
                        .as_ref()
                        .map(|bookmark| {
                            Ok(crate::navigation_history::ViewBookmark {
                                key: bookmark.key.to_entry_key()?,
                                offset: bookmark.offset,
                                selection: bookmark.selection.to_selection()?,
                            })
                        })
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        Ok(crate::navigation_history::NavigationHistorySnapshot {
            entries,
            cursor: self.cursor,
        })
    }

    fn validate(&self) -> Result<(), SessionError> {
        if self.entries.is_empty() || self.entries.len() > MAX_HISTORY_ENTRIES {
            return Err(SessionError::LimitExceeded("closed tab navigation entries"));
        }
        if self.cursor >= self.entries.len() {
            return Err(SessionError::Malformed(
                "closed tab navigation cursor".into(),
            ));
        }
        let mut selected_keys = 0usize;
        for entry in &self.entries {
            entry.location.validate()?;
            if let Some(bookmark) = &entry.bookmark {
                if !bookmark.offset.is_finite() || bookmark.offset < 0.0 {
                    return Err(SessionError::Malformed("bookmark offset".into()));
                }
                bookmark.key.validate()?;
                selected_keys = selected_keys.saturating_add(bookmark.selection.selected.len());
                bookmark.selection.validate()?;
            }
        }
        if selected_keys > MAX_CLOSED_BOOKMARK_SELECTION_KEYS {
            return Err(SessionError::LimitExceeded(
                "closed tab bookmark selection keys",
            ));
        }
        Ok(())
    }
}

fn validate_tab(tab: &TabSnapshot) -> Result<(), SessionError> {
    tab.location.validate()?;
    if !tab.icon_scale.is_finite() || tab.icon_scale <= 0.0 {
        return Err(SessionError::Malformed("icon scale".into()));
    }
    Ok(())
}

/// 一覧 section の grouping。欠けた既存 snapshot は None として読む。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GroupValue {
    #[default]
    None,
    ModifiedDate,
    Kind,
}

impl From<crate::sorting::Column> for ColumnValue {
    fn from(column: crate::sorting::Column) -> Self {
        match column {
            crate::sorting::Column::Name => Self::Name,
            crate::sorting::Column::Kind => Self::Kind,
            crate::sorting::Column::Size => Self::Size,
            crate::sorting::Column::Modified => Self::Modified,
        }
    }
}

impl From<ColumnValue> for crate::sorting::Column {
    fn from(column: ColumnValue) -> Self {
        match column {
            ColumnValue::Name => Self::Name,
            ColumnValue::Kind => Self::Kind,
            ColumnValue::Size => Self::Size,
            ColumnValue::Modified => Self::Modified,
        }
    }
}

impl From<crate::sorting::SortState> for SortValue {
    fn from(sort: crate::sorting::SortState) -> Self {
        match sort {
            crate::sorting::SortState::None => Self::None,
            crate::sorting::SortState::Asc(column) => Self::Asc(column.into()),
            crate::sorting::SortState::Desc(column) => Self::Desc(column.into()),
        }
    }
}

impl From<SortValue> for crate::sorting::SortState {
    fn from(sort: SortValue) -> Self {
        match sort {
            SortValue::None => Self::None,
            SortValue::Asc(column) => Self::Asc(column.into()),
            SortValue::Desc(column) => Self::Desc(column.into()),
        }
    }
}

/// 一つのタブ。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    /// このタブの identity。実行をまたいで同じタブを指す。snapshot 内で一意。
    ///
    /// process-local な `ViewId` は保存しない。将来 workspace ごとの保存に移すときも、
    /// この key がそのまま参照先になる。
    pub key: u64,
    pub location: LocationValue,
    pub layout: LayoutValue,
    pub icon_scale: f32,
    pub sort: SortValue,
    #[serde(default)]
    pub group: GroupValue,
    /// close から保護する v5 互換の tab lock。旧 snapshot は unlocked として復元する。
    #[serde(default)]
    pub locked: bool,
}

/// window の位置と大きさ。最大化中の rect を normal bounds へ流用しない。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundsValue {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// 通常時の bounds。maximized のものを入れない。
    pub normal_bounds: Option<BoundsValue>,
    pub maximized: bool,
    /// 取得時の DPI。復元時に現在の display と突き合わせる材料。
    pub dpi: Option<f32>,
    /// 縦タブバーの幅。旧snapshotでは既定値を使う。
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// Per-window tabs explicitly closed by the user; absent in existing schema-2 files.
    #[serde(default)]
    pub closed_tabs: Vec<ClosedTabSnapshot>,
}

fn default_sidebar_width() -> f32 {
    184.0
}

/// 一つのdirectory paneと、その中の順序付きfolder tabs。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub key: u64,
    pub active: Option<u64>,
    pub tabs: Vec<TabSnapshot>,
}

/// Ordered window workspace payload. IDs are stable within the saved window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: u64,
    pub name: String,
    pub panes: Vec<PaneSnapshot>,
    pub pane_layout: PaneLayoutSnapshot,
    pub focused_pane: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClosedWorkspaceSnapshot {
    pub history_id: u64,
    pub workspace: WorkspaceSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplitDirectionValue {
    Horizontal,
    Vertical,
}

/// Workspace が唯一所有する pane の位置・分割方向・比率。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PaneLayoutSnapshot {
    Pane {
        key: u64,
    },
    Split {
        direction: SplitDirectionValue,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl Default for PaneLayoutSnapshot {
    fn default() -> Self {
        // schema 5 で arrangement が欠けた snapshot は後段の validation で拒否する。
        // schema 1–4 は from_json が保存順から有効な layout を移行する。
        Self::Pane { key: 0 }
    }
}

impl PaneLayoutSnapshot {
    fn linear_horizontal(pane_keys: &[u64]) -> Self {
        match pane_keys {
            [] => Self::Pane { key: 0 },
            [key] => Self::Pane { key: *key },
            [first, rest @ ..] => Self::Split {
                direction: SplitDirectionValue::Horizontal,
                ratio: 1.0 / pane_keys.len() as f32,
                first: Box::new(Self::Pane { key: *first }),
                second: Box::new(Self::linear_horizontal(rest)),
            },
        }
    }

    fn validate_keys(
        &self,
        depth: usize,
        keys: &mut std::collections::HashSet<u64>,
    ) -> Result<(), SessionError> {
        if depth > MAX_PANE_LAYOUT_DEPTH {
            return Err(SessionError::LimitExceeded("pane layout depth"));
        }
        match self {
            Self::Pane { key } => {
                if *key == 0 || !keys.insert(*key) {
                    return Err(SessionError::Malformed(
                        "invalid or duplicate pane layout key".into(),
                    ));
                }
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.02..=0.98).contains(ratio) {
                    return Err(SessionError::Malformed("pane split ratio".into()));
                }
                first.validate_keys(depth + 1, keys)?;
                second.validate_keys(depth + 1, keys)?;
            }
        }
        Ok(())
    }
}

/// One process session containing ordered windows and shared application settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub schema_version: u32,
    /// 単調に増える。古い write が新しい write を置換しないための番号。
    pub write_revision: u64,
    pub windows: Vec<WindowSessionSnapshot>,
    pub active_window: u64,
    #[serde(default = "default_window_id_high_water")]
    pub window_id_high_water: u64,
    pub workspace_id_high_water: u64,
    pub tab_id_high_water: u64,
    /// F2 の初期選択へ最終拡張子を含める application-wide preference。
    /// 既存 schema-2 snapshot は Explorer-like な false として読む。
    #[serde(default)]
    pub select_extension_on_rename: bool,
    /// App-wide hidden-item visibility; snapshots written before this field default to false.
    #[serde(default)]
    pub show_hidden_items: bool,
    /// App-wide per-item checkbox visibility; old snapshots preserve the visible default.
    #[serde(default = "default_item_check_boxes")]
    pub item_check_boxes: bool,
    /// Application-wide theme basename; paths are never persisted here.
    #[serde(default = "default_theme_name")]
    pub theme_name: String,
    /// Application-wide locale override (`auto`, `ja`, or `en`).
    #[serde(default = "default_language_override")]
    pub language_override: String,
    /// 正常終了の印。crash と区別するためのもの。
    pub clean_shutdown: bool,
}

/// Restorable state owned by one window. Identity is stable within the session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSessionSnapshot {
    pub id: u64,
    pub workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default)]
    pub closed_workspaces: Vec<ClosedWorkspaceSnapshot>,
    pub active_workspace: u64,
    pub window: Option<WindowSnapshot>,
}

impl SessionSnapshot {
    /// Runtime compatibility projection for the current single-window shell.
    fn active_window_snapshot(&self) -> &WindowSessionSnapshot {
        self.windows
            .iter()
            .find(|window| window.id == self.active_window)
            .expect("validated session has an active window")
    }
    fn active_window_snapshot_mut(&mut self) -> &mut WindowSessionSnapshot {
        let active = self.active_window;
        self.windows
            .iter_mut()
            .find(|window| window.id == active)
            .expect("validated session has an active window")
    }

    fn maximum_stored_tab_id(&self) -> u64 {
        let mut maximum = 0;
        for window in &self.windows {
            for workspace in window.workspaces.iter().chain(
                window
                    .closed_workspaces
                    .iter()
                    .map(|closed| &closed.workspace),
            ) {
                for tab in workspace.panes.iter().flat_map(|pane| pane.tabs.iter()) {
                    maximum = maximum.max(tab.key);
                }
            }
            if let Some(metadata) = &window.window {
                for tab in &metadata.closed_tabs {
                    maximum = maximum.max(tab.tab.key);
                }
            }
        }
        maximum
    }
}

impl std::ops::Deref for SessionSnapshot {
    type Target = WindowSessionSnapshot;
    fn deref(&self) -> &Self::Target {
        self.active_window_snapshot()
    }
}
impl std::ops::DerefMut for SessionSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.active_window_snapshot_mut()
    }
}

#[derive(Deserialize)]
struct LegacyCurrentSessionSnapshot {
    write_revision: u64,
    workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default)]
    closed_workspaces: Vec<ClosedWorkspaceSnapshot>,
    active_workspace: u64,
    #[serde(default = "default_workspace_id_high_water")]
    workspace_id_high_water: u64,
    window: Option<WindowSnapshot>,
    #[serde(default)]
    select_extension_on_rename: bool,
    #[serde(default = "default_theme_name")]
    theme_name: String,
    #[serde(default = "default_language_override")]
    language_override: String,
    clean_shutdown: bool,
}

impl LegacyCurrentSessionSnapshot {
    fn into_current(self) -> SessionSnapshot {
        let tab_id_high_water = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.panes.iter())
            .flat_map(|pane| pane.tabs.iter())
            .map(|tab| tab.key)
            .max()
            .unwrap_or(1);
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            write_revision: self.write_revision,
            windows: vec![WindowSessionSnapshot {
                id: 1,
                workspaces: self.workspaces,
                closed_workspaces: self.closed_workspaces,
                active_workspace: self.active_workspace,
                window: self.window,
            }],
            active_window: 1,
            window_id_high_water: 1,
            workspace_id_high_water: self.workspace_id_high_water,
            tab_id_high_water,
            select_extension_on_rename: self.select_extension_on_rename,
            show_hidden_items: false,
            item_check_boxes: true,
            theme_name: self.theme_name,
            language_override: self.language_override,
            clean_shutdown: self.clean_shutdown,
        }
    }
}

impl SessionSnapshot {
    /// byte 数を見てから parse し、内容を検証する。
    pub fn from_json(bytes: &[u8]) -> Result<Self, SessionError> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(SessionError::LimitExceeded("snapshot bytes"));
        }
        #[derive(Deserialize)]
        struct SchemaVersion {
            schema_version: u32,
        }
        let version: SchemaVersion = serde_json::from_slice(bytes)
            .map_err(|error| SessionError::Malformed(error.to_string()))?;
        let snapshot = match version.schema_version {
            1 => {
                let legacy: LegacySessionSnapshot = serde_json::from_slice(bytes)
                    .map_err(|error| SessionError::Malformed(error.to_string()))?;
                legacy.into_current()
            }
            2 | 3 => {
                let previous: LegacyFlatSessionSnapshot = serde_json::from_slice(bytes)
                    .map_err(|error| SessionError::Malformed(error.to_string()))?;
                previous.into_current()
            }
            4..=7 => {
                let previous: LegacyPaneSessionSnapshot = serde_json::from_slice(bytes)
                    .map_err(|error| SessionError::Malformed(error.to_string()))?;
                previous.into_current()
            }
            8 | 9 => {
                let previous: LegacyCurrentSessionSnapshot = serde_json::from_slice(bytes)
                    .map_err(|error| SessionError::Malformed(error.to_string()))?;
                previous.into_current()
            }
            10 | SCHEMA_VERSION => serde_json::from_slice(bytes)
                .map_err(|error| SessionError::Malformed(error.to_string()))?,
            found if found > SCHEMA_VERSION => {
                return Err(SessionError::FutureSchema {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
            found => {
                return Err(SessionError::Malformed(format!(
                    "unsupported session schema {found}"
                )));
            }
        };
        let mut snapshot = snapshot;
        if version.schema_version < 10 {
            if version.schema_version <= 4 {
                let workspace = &mut snapshot.workspaces[0];
                let pane_keys = workspace
                    .panes
                    .iter()
                    .map(|pane| pane.key)
                    .collect::<Vec<_>>();
                workspace.pane_layout = PaneLayoutSnapshot::linear_horizontal(&pane_keys);
            }
            if snapshot.theme_name.is_empty() {
                snapshot.theme_name = default_theme_name();
            }
            if version.schema_version <= 8 {
                snapshot.workspace_id_high_water = snapshot
                    .windows
                    .iter()
                    .flat_map(|window| {
                        window
                            .workspaces
                            .iter()
                            .map(|workspace| workspace.id)
                            .chain(
                                window
                                    .closed_workspaces
                                    .iter()
                                    .map(|closed| closed.workspace.id),
                            )
                    })
                    .max()
                    .unwrap_or(1);
            }
            snapshot.window_id_high_water = 1;
            snapshot.tab_id_high_water = snapshot.maximum_stored_tab_id().max(1);
            snapshot.schema_version = SCHEMA_VERSION;
        } else if version.schema_version == 10 {
            snapshot.schema_version = SCHEMA_VERSION;
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// 検証してから直列化する。壊れた snapshot を書かない。
    pub fn to_json(&self) -> Result<Vec<u8>, SessionError> {
        self.validate()?;
        let bytes =
            serde_json::to_vec(self).map_err(|error| SessionError::Malformed(error.to_string()))?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(SessionError::LimitExceeded("snapshot bytes"));
        }
        Ok(bytes)
    }

    /// 保存できる内容か。
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.schema_version > SCHEMA_VERSION {
            return Err(SessionError::FutureSchema {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(SessionError::Malformed(format!(
                "unsupported session schema {}",
                self.schema_version
            )));
        }
        if self.windows.is_empty() {
            return Err(SessionError::Malformed("session has no windows".into()));
        }
        if self.windows.len() > MAX_WINDOWS {
            return Err(SessionError::LimitExceeded("window count"));
        }
        if self.window_id_high_water == 0
            || self
                .windows
                .iter()
                .any(|window| window.id == 0 || window.id > self.window_id_high_water)
        {
            return Err(SessionError::Malformed(
                "window ID high-water mark is invalid".into(),
            ));
        }
        let mut window_ids = std::collections::HashSet::new();
        if self
            .windows
            .iter()
            .any(|window| !window_ids.insert(window.id))
            || !window_ids.contains(&self.active_window)
        {
            return Err(SessionError::Malformed(
                "invalid or duplicate window ID, or active window is missing".into(),
            ));
        }
        let all_workspaces = self
            .windows
            .iter()
            .flat_map(|window| window.workspaces.iter());
        let all_closed_workspaces = self
            .windows
            .iter()
            .flat_map(|window| window.closed_workspaces.iter());
        if self.workspace_id_high_water == 0
            || all_workspaces
                .clone()
                .map(|workspace| workspace.id)
                .chain(
                    all_closed_workspaces
                        .clone()
                        .map(|closed| closed.workspace.id),
                )
                .any(|id| id > self.workspace_id_high_water)
        {
            return Err(SessionError::Malformed(
                "workspace ID high-water mark is invalid".into(),
            ));
        }
        if self.tab_id_high_water == 0 {
            return Err(SessionError::Malformed(
                "tab ID high-water mark is invalid".into(),
            ));
        }
        let mut total_workspaces = 0usize;
        let mut total_closed_workspaces = 0usize;
        let mut total_closed_tabs = 0usize;
        let mut closed_tab_bytes = 0usize;
        let mut closed_workspace_bytes = 0usize;
        let mut aggregate_panes = 0usize;
        let mut seen = std::collections::HashSet::new();
        let mut tab_count = 0usize;
        let mut workspace_ids = std::collections::HashSet::new();
        for window in &self.windows {
            total_workspaces += window.workspaces.len();
            total_closed_workspaces += window.closed_workspaces.len();
            if window.closed_workspaces.len() > MAX_CLOSED_WORKSPACES {
                return Err(SessionError::LimitExceeded("closed workspace count"));
            }
            let mut closed_workspace_ids = std::collections::HashSet::new();
            for closed in &window.closed_workspaces {
                if closed.history_id == 0 || !closed_workspace_ids.insert(closed.history_id) {
                    return Err(SessionError::Malformed(
                        "invalid or duplicate closed workspace history ID".into(),
                    ));
                }
                let workspace = &closed.workspace;
                // Recovery DTOs are validated as a one-workspace session so they share
                // the live pane/tab/layout constraints without weakening either contract.
                let recovered = SessionSnapshot {
                    schema_version: SCHEMA_VERSION,
                    write_revision: self.write_revision,
                    windows: vec![WindowSessionSnapshot {
                        id: 1,
                        workspaces: vec![workspace.clone()],
                        closed_workspaces: Vec::new(),
                        active_workspace: workspace.id,
                        window: None,
                    }],
                    active_window: 1,
                    window_id_high_water: 1,
                    workspace_id_high_water: workspace.id,
                    tab_id_high_water: self.tab_id_high_water,
                    select_extension_on_rename: self.select_extension_on_rename,
                    show_hidden_items: false,
                    item_check_boxes: true,
                    theme_name: self.theme_name.clone(),
                    language_override: self.language_override.clone(),
                    clean_shutdown: self.clean_shutdown,
                };
                recovered.validate()?;
                let encoded = serde_json::to_vec(closed)
                    .map_err(|error| SessionError::Malformed(error.to_string()))?;
                closed_workspace_bytes = closed_workspace_bytes.saturating_add(encoded.len());
                if closed_workspace_bytes > MAX_CLOSED_WORKSPACE_HISTORY_BYTES {
                    return Err(SessionError::LimitExceeded(
                        "closed workspace history bytes",
                    ));
                }
            }
            if window.workspaces.is_empty() {
                return Err(SessionError::Malformed("window has no workspaces".into()));
            }
            if window.workspaces.len() > MAX_WORKSPACES {
                return Err(SessionError::LimitExceeded("workspace count"));
            }
            if window.active_workspace == 0
                || !window
                    .workspaces
                    .iter()
                    .any(|w| w.id == window.active_workspace)
            {
                return Err(SessionError::Malformed(
                    "active workspace is missing".into(),
                ));
            }
            for workspace in &window.workspaces {
                if workspace.name.is_empty() || workspace.name.trim().is_empty() {
                    return Err(SessionError::Malformed("empty workspace name".into()));
                }
                if workspace.name.chars().any(char::is_control) {
                    return Err(SessionError::Malformed(
                        "workspace name contains control characters".into(),
                    ));
                }
                if workspace.name.len() > MAX_WORKSPACE_NAME_BYTES {
                    return Err(SessionError::LimitExceeded("workspace name"));
                }
                if workspace.id == 0 || !workspace_ids.insert(workspace.id) {
                    return Err(SessionError::Malformed(
                        "invalid or duplicate workspace ID".into(),
                    ));
                }
                if workspace.panes.is_empty() {
                    return Err(SessionError::Malformed("workspace has no panes".into()));
                }
                aggregate_panes = aggregate_panes.saturating_add(workspace.panes.len());
                if aggregate_panes > MAX_PANES {
                    return Err(SessionError::LimitExceeded("pane count"));
                }
                let mut pane_keys = std::collections::HashSet::with_capacity(workspace.panes.len());
                let mut focused_exists = false;
                for pane in &workspace.panes {
                    if pane.key == 0 || !pane_keys.insert(pane.key) {
                        return Err(SessionError::Malformed(
                            "invalid or duplicate pane key".into(),
                        ));
                    }
                    focused_exists |= pane.key == workspace.focused_pane;
                    if pane.tabs.is_empty() {
                        return Err(SessionError::Malformed("empty pane".into()));
                    }
                    tab_count = tab_count.saturating_add(pane.tabs.len());
                    if pane
                        .active
                        .is_none_or(|active| !pane.tabs.iter().any(|tab| tab.key == active))
                    {
                        return Err(SessionError::Malformed("pane active tab is missing".into()));
                    }
                    for tab in &pane.tabs {
                        if !seen.insert(tab.key) {
                            return Err(SessionError::DuplicateTabKey(tab.key));
                        }
                        tab.location.validate()?;
                        if !tab.icon_scale.is_finite() || tab.icon_scale <= 0.0 {
                            return Err(SessionError::Malformed("icon scale".into()));
                        }
                    }
                }
                if !focused_exists {
                    return Err(SessionError::Malformed("focused pane is missing".into()));
                }
                let mut layout_keys =
                    std::collections::HashSet::with_capacity(workspace.panes.len());
                workspace.pane_layout.validate_keys(0, &mut layout_keys)?;
                if layout_keys != pane_keys {
                    return Err(SessionError::Malformed(
                        "pane layout does not match saved panes".into(),
                    ));
                }
            }
            if tab_count > MAX_TABS {
                return Err(SessionError::LimitExceeded("tab count"));
            }
            if let Some(window_meta) = &window.window {
                if let Some(bounds) = &window_meta.normal_bounds {
                    let finite = bounds.x.is_finite()
                        && bounds.y.is_finite()
                        && bounds.width.is_finite()
                        && bounds.height.is_finite();
                    if !finite || bounds.width <= 0.0 || bounds.height <= 0.0 {
                        return Err(SessionError::Malformed("window bounds".into()));
                    }
                }
                if window_meta
                    .dpi
                    .is_some_and(|dpi| !dpi.is_finite() || dpi <= 0.0)
                {
                    return Err(SessionError::Malformed("window dpi".into()));
                }
                total_closed_tabs += window_meta.closed_tabs.len();
                for closed in &window_meta.closed_tabs {
                    closed.validate()?;
                    closed_tab_bytes =
                        closed_tab_bytes.saturating_add(closed.estimated_stored_size()?);
                    if closed_tab_bytes > MAX_CLOSED_HISTORY_BYTES {
                        return Err(SessionError::LimitExceeded("closed history bytes"));
                    }
                }
            }
        }
        if total_workspaces > MAX_WORKSPACES {
            return Err(SessionError::LimitExceeded("workspace count"));
        }
        if total_closed_workspaces > MAX_CLOSED_WORKSPACES {
            return Err(SessionError::LimitExceeded("closed workspace count"));
        }
        if total_closed_tabs > MAX_CLOSED_TABS {
            return Err(SessionError::LimitExceeded("closed tab count"));
        }
        if self.tab_id_high_water < self.maximum_stored_tab_id() {
            return Err(SessionError::Malformed(
                "tab ID high-water mark is invalid".into(),
            ));
        }
        Ok(())
    }
}

fn default_theme_name() -> String {
    "default".into()
}

fn validate_archive_components(components: &[String]) -> Result<(), SessionError> {
    if components.len() > MAX_ARCHIVE_COMPONENTS {
        return Err(SessionError::LimitExceeded("archive address depth"));
    }
    let mut bytes = 0usize;
    for component in components {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component
                .chars()
                .any(|character| matches!(character, '/' | '\\' | '\0'))
        {
            return Err(SessionError::Malformed(
                "invalid archive address component".into(),
            ));
        }
        validate_nonempty_bounded_text(component, "archive address component")?;
        bytes = bytes.saturating_add(component.len());
    }
    if bytes > MAX_ENTRY_IDENTITY_BYTES {
        return Err(SessionError::LimitExceeded("archive address"));
    }
    Ok(())
}

fn default_item_check_boxes() -> bool {
    true
}

fn default_workspace_id_high_water() -> u64 {
    1
}

fn default_window_id_high_water() -> u64 {
    1
}

fn default_language_override() -> String {
    "auto".into()
}

fn migrated_workspace(
    panes: Vec<PaneSnapshot>,
    pane_layout: PaneLayoutSnapshot,
    focused_pane: u64,
) -> Vec<WorkspaceSnapshot> {
    vec![WorkspaceSnapshot {
        id: 1,
        name: "Workspace 1".into(),
        panes,
        pane_layout,
        focused_pane,
    }]
}

/// v1 は tab location を path そのものとして保存していた。
#[derive(Deserialize)]
struct LegacySessionSnapshot {
    schema_version: u32,
    write_revision: u64,
    active: Option<u64>,
    tabs: Vec<LegacyTabSnapshot>,
    window: Option<WindowSnapshot>,
    clean_shutdown: bool,
}

#[derive(Deserialize)]
struct LegacyTabSnapshot {
    key: u64,
    location: PathValue,
    layout: LayoutValue,
    icon_scale: f32,
    sort: SortValue,
}

impl LegacySessionSnapshot {
    fn into_current(self) -> SessionSnapshot {
        debug_assert_eq!(self.schema_version, 1);
        let keys = self.tabs.iter().map(|tab| tab.key).collect::<Vec<_>>();
        let active = legacy_active(self.active, &keys);
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            write_revision: self.write_revision,
            windows: vec![WindowSessionSnapshot {
                id: 1,
                workspaces: migrated_workspace(
                    vec![PaneSnapshot {
                        key: 1,
                        active,
                        tabs: self
                            .tabs
                            .into_iter()
                            .map(|tab| TabSnapshot {
                                key: tab.key,
                                location: LocationValue::Path { path: tab.location },
                                layout: tab.layout,
                                icon_scale: tab.icon_scale,
                                sort: tab.sort,
                                group: GroupValue::None,
                                locked: false,
                            })
                            .collect(),
                    }],
                    PaneLayoutSnapshot::default(),
                    1,
                ),
                closed_workspaces: Vec::new(),
                active_workspace: 1,
                window: self.window,
            }],
            active_window: 1,
            window_id_high_water: 1,
            workspace_id_high_water: 1,
            tab_id_high_water: keys.iter().copied().max().unwrap_or(1),
            select_extension_on_rename: false,
            show_hidden_items: false,
            item_check_boxes: true,
            theme_name: "default".into(),
            language_override: default_language_override(),
            clean_shutdown: self.clean_shutdown,
        }
    }
}

#[derive(Deserialize)]
struct LegacyFlatSessionSnapshot {
    #[serde(default)]
    write_revision: u64,
    active: Option<u64>,
    tabs: Vec<TabSnapshot>,
    #[serde(default)]
    window: Option<WindowSnapshot>,
    #[serde(default)]
    select_extension_on_rename: bool,
    #[serde(default)]
    clean_shutdown: bool,
}

impl LegacyFlatSessionSnapshot {
    fn into_current(self) -> SessionSnapshot {
        let tab_id_high_water = self.tabs.iter().map(|tab| tab.key).max().unwrap_or(1);
        let active = legacy_active(
            self.active,
            &self.tabs.iter().map(|tab| tab.key).collect::<Vec<_>>(),
        );
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            write_revision: self.write_revision,
            windows: vec![WindowSessionSnapshot {
                id: 1,
                workspaces: migrated_workspace(
                    vec![PaneSnapshot {
                        key: 1,
                        active,
                        tabs: self.tabs,
                    }],
                    PaneLayoutSnapshot::default(),
                    1,
                ),
                closed_workspaces: Vec::new(),
                active_workspace: 1,
                window: self.window,
            }],
            active_window: 1,
            window_id_high_water: 1,
            workspace_id_high_water: 1,
            tab_id_high_water,
            select_extension_on_rename: self.select_extension_on_rename,
            show_hidden_items: false,
            item_check_boxes: true,
            theme_name: "default".into(),
            language_override: default_language_override(),
            clean_shutdown: self.clean_shutdown,
        }
    }
}

/// Schemas 4–7 stored one pane collection directly on the session.
#[derive(Deserialize)]
struct LegacyPaneSessionSnapshot {
    schema_version: u32,
    write_revision: u64,
    panes: Vec<PaneSnapshot>,
    #[serde(default)]
    pane_layout: PaneLayoutSnapshot,
    focused_pane: u64,
    #[serde(default)]
    window: Option<WindowSnapshot>,
    #[serde(default)]
    select_extension_on_rename: bool,
    #[serde(default = "default_theme_name")]
    theme_name: String,
    #[serde(default = "default_language_override")]
    language_override: String,
    clean_shutdown: bool,
}

impl LegacyPaneSessionSnapshot {
    fn into_current(self) -> SessionSnapshot {
        let pane_layout = if self.schema_version <= 4 {
            PaneLayoutSnapshot::linear_horizontal(
                &self.panes.iter().map(|pane| pane.key).collect::<Vec<_>>(),
            )
        } else {
            self.pane_layout
        };
        let tab_id_high_water = self
            .panes
            .iter()
            .flat_map(|pane| pane.tabs.iter())
            .map(|tab| tab.key)
            .max()
            .unwrap_or(1);
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            write_revision: self.write_revision,
            windows: vec![WindowSessionSnapshot {
                id: 1,
                workspaces: migrated_workspace(self.panes, pane_layout, self.focused_pane),
                closed_workspaces: Vec::new(),
                active_workspace: 1,
                window: self.window,
            }],
            active_window: 1,
            window_id_high_water: 1,
            workspace_id_high_water: 1,
            tab_id_high_water,
            select_extension_on_rename: self.select_extension_on_rename,
            show_hidden_items: false,
            item_check_boxes: true,
            theme_name: self.theme_name,
            language_override: self.language_override,
            clean_shutdown: self.clean_shutdown,
        }
    }
}

fn legacy_active(active: Option<u64>, keys: &[u64]) -> Option<u64> {
    active
        .filter(|key| keys.contains(key))
        .or_else(|| keys.first().copied())
}

/// 保存先。filesystem を差し替えられるように trait にする。
pub trait SessionSink {
    /// 全内容を書く。失敗の理由は可視の診断に使う。
    fn write(&mut self, bytes: &[u8]) -> Result<(), String>;
}

/// 直列 writer。待機中の要求は最新一つへ畳む。
///
/// **古い write が新しい write の後に disk を置換しない**ことが契約。同時に走る
/// write を常に一つへ限り、待機分を最新へ畳むことで満たす。UI 側で古い完了通知を
/// 捨てるだけでは、disk の巻戻りは防げない。
#[cfg(test)]
pub struct OrderedWriter<S: SessionSink> {
    sink: S,
    /// まだ書いていない最新の要求。
    pending: Option<(u64, Vec<u8>)>,
    /// disk に載った最新 revision。
    written: u64,
    /// 直前の失敗。再試行の判断に使う。
    last_failure: Option<String>,
}

/// Background serial owner for session snapshot replacement.
///
/// Each request is an ordering barrier: unlike ordinary snapshot coalescing,
/// callers that need a durable acknowledgement must never have their request
/// replaced by a later autosave before the worker writes it.
pub struct AsyncOrderedWriter {
    sender: async_channel::Sender<AsyncWriteRequest>,
}

struct AsyncWriteRequest {
    revision: u64,
    bytes: Vec<u8>,
    reply: async_channel::Sender<Result<u64, String>>,
}

impl AsyncOrderedWriter {
    pub fn new<S: SessionSink + Send + 'static>(mut sink: S) -> Self {
        let (sender, receiver) = async_channel::unbounded::<AsyncWriteRequest>();
        std::thread::Builder::new()
            .name("session-snapshot-writer".into())
            .spawn(move || {
                let mut written = 0;
                while let Ok(request) = receiver.recv_blocking() {
                    let result = if request.revision <= written {
                        Err(format!(
                            "session writer rejected non-increasing revision {} after {written}",
                            request.revision
                        ))
                    } else {
                        match sink.write(&request.bytes) {
                            Ok(()) => {
                                written = request.revision;
                                Ok(written)
                            }
                            Err(reason) => Err(reason),
                        }
                    };
                    let _ = request.reply.send_blocking(result);
                }
            })
            .expect("session snapshot writer thread must start");
        Self { sender }
    }

    /// Enqueue one ordered write and return its nonblocking durable receipt.
    pub fn write(
        &self,
        revision: u64,
        bytes: Vec<u8>,
    ) -> async_channel::Receiver<Result<u64, String>> {
        let (reply, receipt) = async_channel::bounded(1);
        if self
            .sender
            .send_blocking(AsyncWriteRequest {
                revision,
                bytes,
                reply: reply.clone(),
            })
            .is_err()
        {
            let _ = reply.send_blocking(Err("session snapshot writer is unavailable".into()));
        }
        receipt
    }
}

#[cfg(test)]
impl<S: SessionSink> OrderedWriter<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            pending: None,
            written: 0,
            last_failure: None,
        }
    }

    /// 保存を要求する。古い revision は捨てる。
    ///
    /// 待機中に新しい要求が来たら、古い方の bytes は書かずに捨てる。
    pub fn request(&mut self, revision: u64, bytes: Vec<u8>) {
        if revision <= self.written {
            // 既に載っている版より古い。書く意味がない。
            return;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|(queued, _)| *queued >= revision)
        {
            return;
        }
        self.pending = Some((revision, bytes));
    }

    /// 未保存の要求があるか。
    #[allow(dead_code)] // 畳み込みの契約をテストから確かめる。
    pub fn is_dirty(&self) -> bool {
        self.pending.is_some()
    }

    /// 一つ書く。待機が無ければ `None`。
    ///
    /// 失敗しても要求は残す。成功したときだけ `written` を進める。
    pub fn flush(&mut self) -> Option<Result<u64, String>> {
        let (revision, bytes) = self.pending.clone()?;
        match self.sink.write(&bytes) {
            Ok(()) => {
                self.pending = None;
                self.written = revision;
                self.last_failure = None;
                Some(Ok(revision))
            }
            Err(reason) => {
                self.last_failure = Some(reason.clone());
                Some(Err(reason))
            }
        }
    }

    /// disk に載っている最新 revision。
    #[allow(dead_code)] // テストから確かめる。
    pub fn written_revision(&self) -> u64 {
        self.written
    }

    #[allow(dead_code)] // テストから確かめる。
    pub fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    #[allow(dead_code)] // テストから確かめる。
    pub fn sink(&self) -> &S {
        &self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(key: u64, name: &str) -> TabSnapshot {
        TabSnapshot {
            key,
            location: LocationValue::Path {
                path: PathValue::from_path(Path::new(name)).unwrap(),
            },
            layout: LayoutValue::Rows,
            icon_scale: 1.25,
            sort: SortValue::None,
            group: GroupValue::None,
            locked: false,
        }
    }

    fn snapshot(tabs: Vec<TabSnapshot>) -> SessionSnapshot {
        let active = tabs.first().map(|tab| tab.key);
        let tab_id_high_water = tabs.iter().map(|tab| tab.key).max().unwrap_or(1);
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            write_revision: 1,
            windows: vec![WindowSessionSnapshot {
                id: 1,
                workspaces: migrated_workspace(
                    vec![PaneSnapshot {
                        key: 1,
                        active,
                        tabs,
                    }],
                    PaneLayoutSnapshot::Pane { key: 1 },
                    1,
                ),
                closed_workspaces: Vec::new(),
                active_workspace: 1,
                window: Some(WindowSnapshot {
                    normal_bounds: Some(BoundsValue {
                        x: 10.0,
                        y: 20.0,
                        width: 840.0,
                        height: 620.0,
                    }),
                    maximized: false,
                    dpi: Some(2.0),
                    sidebar_width: default_sidebar_width(),
                    closed_tabs: Vec::new(),
                }),
            }],
            active_window: 1,
            window_id_high_water: 1,
            workspace_id_high_water: 1,
            tab_id_high_water,
            select_extension_on_rename: false,
            show_hidden_items: false,
            item_check_boxes: true,
            theme_name: "default".into(),
            language_override: default_language_override(),
            clean_shutdown: true,
        }
    }

    #[test]
    fn archive_location_and_bookmark_key_round_trip_losslessly() {
        let location = crate::location::Location::Archive {
            source: PathBuf::from(r"C:\work\bundle.zip"),
            components: vec!["nested".into(), "empty".into()],
        };
        let value = LocationValue::from_location(&location).unwrap();
        assert_eq!(value.to_location().unwrap(), location);
        let key = crate::entry_identity::EntryKey::Archive {
            source: PathBuf::from(r"C:\work\bundle.zip"),
            components: vec!["nested".into(), "empty".into()],
            kind: crate::directory::EntryKind::Folder,
        };
        let value = EntryKeyValue::from_entry_key(&key).unwrap();
        assert_eq!(value.to_entry_key().unwrap(), key);
    }

    #[test]
    fn schema_ten_migration_preserves_typed_archive_address() {
        let location = crate::location::Location::Archive {
            source: PathBuf::from(r"C:\work\bundle.zip"),
            components: vec!["nested".into()],
        };
        let mut entry = tab(1, r"C:\unused");
        entry.location = LocationValue::from_location(&location).unwrap();
        let mut original = snapshot(vec![entry]);
        original.schema_version = 10;
        let restored = SessionSnapshot::from_json(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(restored.schema_version, SCHEMA_VERSION);
        assert_eq!(
            restored.workspaces[0].panes[0].tabs[0]
                .location
                .to_location()
                .unwrap(),
            location
        );
    }

    #[test]
    fn schema_four_panes_migrate_to_an_equal_horizontal_layout() {
        let mut original = snapshot(vec![tab(1, r"C:\work\alpha")]);
        original.schema_version = 4;
        original.workspaces[0].panes.push(PaneSnapshot {
            key: 2,
            active: Some(2),
            tabs: vec![tab(2, r"C:\work\beta")],
        });
        original.workspaces[0].focused_pane = 2;
        let mut value = serde_json::to_value(original).unwrap();
        let workspace = value["windows"][0]["workspaces"][0].clone();
        let root = value.as_object_mut().unwrap();
        root.remove("workspaces");
        root.remove("active_workspace");
        root.remove("windows");
        root.remove("active_window");
        root.remove("window_id_high_water");
        root.remove("workspace_id_high_water");
        root.remove("tab_id_high_water");
        root.remove("pane_layout");
        root.insert("panes".into(), workspace["panes"].clone());
        root.insert("focused_pane".into(), workspace["focused_pane"].clone());

        let restored = SessionSnapshot::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(restored.schema_version, SCHEMA_VERSION);
        assert_eq!(restored.workspaces[0].focused_pane, 2);
        assert_eq!(
            restored.workspaces[0].pane_layout,
            PaneLayoutSnapshot::Split {
                direction: SplitDirectionValue::Horizontal,
                ratio: 0.5,
                first: Box::new(PaneLayoutSnapshot::Pane { key: 1 }),
                second: Box::new(PaneLayoutSnapshot::Pane { key: 2 }),
            }
        );
    }

    #[test]
    fn current_schema_requires_a_matching_pane_layout_tree() {
        let original = snapshot(vec![tab(1, r"C:\work\alpha")]);
        let mut value = serde_json::to_value(original).unwrap();
        value["windows"][0]["workspaces"][0]
            .as_object_mut()
            .unwrap()
            .remove("pane_layout");

        assert!(matches!(
            SessionSnapshot::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(SessionError::Malformed(_))
        ));
    }

    #[test]
    fn a_snapshot_round_trips() {
        let mut original = snapshot(vec![
            tab(1, r"C:\work\alpha"),
            tab(7, r"\\server\share\beta"),
        ]);
        original.select_extension_on_rename = true;
        original.show_hidden_items = true;
        original.item_check_boxes = false;
        let bytes = original.to_json().unwrap();
        let restored = SessionSnapshot::from_json(&bytes).unwrap();
        assert_eq!(restored, original);
        // 表示順と active key がそのまま残る。
        assert_eq!(restored.workspaces[0].panes[0].active, Some(1));
        assert_eq!(restored.workspaces[0].panes[0].tabs.len(), 2);
        assert!(restored.select_extension_on_rename);
        assert!(restored.show_hidden_items);
        assert!(!restored.item_check_boxes);
    }

    #[test]
    fn schema_two_without_rename_preference_uses_the_safe_default() {
        let original = snapshot(vec![tab(1, r"C:\work")]);
        let mut value = serde_json::to_value(original).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("select_extension_on_rename");
        value.as_object_mut().unwrap().remove("show_hidden_items");
        value.as_object_mut().unwrap().remove("item_check_boxes");

        let restored = SessionSnapshot::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(!restored.select_extension_on_rename);
        assert!(!restored.show_hidden_items);
        assert!(restored.item_check_boxes);
    }

    #[test]
    fn mixed_path_and_this_pc_locations_round_trip() {
        let original = snapshot(vec![
            tab(1, r"C:\work"),
            TabSnapshot {
                key: 2,
                location: LocationValue::ThisPc,
                layout: LayoutValue::Tiles,
                icon_scale: 1.75,
                sort: SortValue::None,
                group: GroupValue::None,
                locked: true,
            },
        ]);

        let bytes = original.to_json().unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains(r#""kind":"this-pc""#)
        );
        assert_eq!(SessionSnapshot::from_json(&bytes).unwrap(), original);
    }

    #[test]
    fn libraries_root_round_trips_as_a_tagged_location() {
        let value = LocationValue::from_location(&crate::location::Location::ShellRoot(
            crate::location::ShellRoot::Libraries,
        ))
        .unwrap();
        assert_eq!(value, LocationValue::Libraries);
        assert_eq!(
            value.to_location().unwrap(),
            crate::location::Location::ShellRoot(crate::location::ShellRoot::Libraries)
        );
    }

    #[test]
    fn named_shell_roots_round_trip_but_shell_children_do_not_persist() {
        use crate::location::ShellRoot;

        for root in [ShellRoot::Network, ShellRoot::RecycleBin] {
            let location = crate::location::Location::ShellRoot(root);
            let value = LocationValue::from_location(&location).unwrap();
            assert_eq!(value.to_location().unwrap(), location);
        }
        let child = crate::location::Location::ShellItem {
            root: ShellRoot::RecycleBin,
            parsing_name: "::{recycle-bin}\\item".into(),
            display_name: "item".into(),
        };
        let value = LocationValue::from_location(&child).unwrap();
        value.validate().unwrap();
        assert_eq!(value.to_location().unwrap(), child);
    }

    #[test]
    fn a_v1_path_snapshot_migrates_to_a_tagged_location() {
        let v1 = br#"{
            "schema_version": 1,
            "write_revision": 7,
            "active": 4,
            "tabs": [{
                "key": 4,
                "location": {"encoding": "windows-utf16", "units": [67, 58, 92, 119, 111, 114, 107]},
                "layout": "rows",
                "icon_scale": 1.0,
                "sort": {"state": "none"}
            }],
            "window": null,
            "clean_shutdown": true
        }"#;

        let restored = SessionSnapshot::from_json(v1).unwrap();
        assert_eq!(restored.schema_version, SCHEMA_VERSION);
        assert_eq!(restored.workspaces[0].panes[0].active, Some(4));
        assert_eq!(
            restored.workspaces[0].panes[0].tabs[0]
                .location
                .to_location()
                .unwrap(),
            crate::location::Location::Path(PathBuf::from(r"C:\work"))
        );
    }

    /// 孤立 surrogate を含む path。`to_string_lossy` を通せば置換文字になる。
    #[test]
    fn a_path_with_an_isolated_surrogate_round_trips() {
        let units = vec![
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            b'a' as u16,
            0xD800,
            b'b' as u16,
        ];
        let value = PathValue::WindowsUtf16 {
            units: units.clone(),
        };
        value.validate().unwrap();
        let bytes = serde_json::to_vec(&value).unwrap();
        let restored: PathValue = serde_json::from_slice(&bytes).unwrap();
        let PathValue::WindowsUtf16 { units: restored } = restored;
        assert_eq!(restored, units);
        // lossy な文字列に置き換わっていないこと。
        assert_eq!(restored[4], 0xD800);
    }

    #[test]
    fn a_future_schema_is_refused() {
        let mut newer = snapshot(vec![tab(1, r"C:\work")]);
        newer.schema_version = SCHEMA_VERSION + 1;
        let bytes = serde_json::to_vec(&newer).unwrap();
        assert_eq!(
            SessionSnapshot::from_json(&bytes),
            Err(SessionError::FutureSchema {
                found: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION,
            })
        );
    }

    #[test]
    fn limits_are_refused_instead_of_trimmed() {
        // tab 数。
        let many = (0..MAX_TABS + 1)
            .map(|index| tab(index as u64, &format!(r"C:\work\{index}")))
            .collect();
        assert_eq!(
            snapshot(many).validate(),
            Err(SessionError::LimitExceeded("tab count"))
        );

        // path の長さ。
        let long = format!(r"C:\{}", "a".repeat(MAX_PATH_UNITS));
        assert_eq!(
            PathValue::from_path(Path::new(&long)),
            Err(SessionError::LimitExceeded("path length"))
        );

        // 入力の byte 数。
        let bytes = vec![b' '; MAX_SNAPSHOT_BYTES + 1];
        assert_eq!(
            SessionSnapshot::from_json(&bytes),
            Err(SessionError::LimitExceeded("snapshot bytes"))
        );
    }

    #[test]
    fn a_duplicate_tab_key_is_refused() {
        let duplicated = snapshot(vec![tab(3, r"C:\work\alpha"), tab(3, r"C:\work\beta")]);
        assert_eq!(duplicated.validate(), Err(SessionError::DuplicateTabKey(3)));
    }

    #[test]
    fn invalid_paths_are_refused() {
        // 空。
        assert_eq!(
            PathValue::WindowsUtf16 { units: Vec::new() }.validate(),
            Err(SessionError::InvalidPath("empty".into()))
        );
        // 相対。
        assert_eq!(
            PathValue::WindowsUtf16 {
                units: r"work\alpha".encode_utf16().collect()
            }
            .validate(),
            Err(SessionError::InvalidPath("relative".into()))
        );
        // NUL。
        assert_eq!(
            PathValue::WindowsUtf16 {
                units: vec![b'C' as u16, b':' as u16, b'\\' as u16, 0]
            }
            .validate(),
            Err(SessionError::InvalidPath("NUL".into()))
        );
        // drive 絶対と UNC は通る。filesystem の存在は見ない。
        for name in [r"C:\", r"C:\missing\folder", r"\\server\share"] {
            assert!(
                PathValue::from_path(Path::new(name)).is_ok(),
                "{name} should be accepted"
            );
        }
        let invalid = snapshot(vec![TabSnapshot {
            key: 1,
            location: LocationValue::Path {
                path: PathValue::WindowsUtf16 {
                    units: r"relative\path".encode_utf16().collect(),
                },
            },
            layout: LayoutValue::Rows,
            icon_scale: 1.0,
            sort: SortValue::None,
            group: GroupValue::None,
            locked: false,
        }]);
        assert_eq!(
            invalid.validate(),
            Err(SessionError::InvalidPath("relative".into()))
        );
    }

    #[test]
    fn invalid_window_values_are_refused() {
        let mut broken = snapshot(vec![tab(1, r"C:\work")]);
        broken.window = Some(WindowSnapshot {
            normal_bounds: Some(BoundsValue {
                x: f32::NAN,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            }),
            maximized: false,
            dpi: None,
            sidebar_width: default_sidebar_width(),
            closed_tabs: Vec::new(),
        });
        assert_eq!(
            broken.validate(),
            Err(SessionError::Malformed("window bounds".into()))
        );

        broken.window = Some(WindowSnapshot {
            normal_bounds: None,
            maximized: true,
            dpi: Some(0.0),
            sidebar_width: default_sidebar_width(),
            closed_tabs: Vec::new(),
        });
        assert_eq!(
            broken.validate(),
            Err(SessionError::Malformed("window dpi".into()))
        );
    }

    #[test]
    fn malformed_json_is_refused() {
        assert!(matches!(
            SessionSnapshot::from_json(b"not json"),
            Err(SessionError::Malformed(_))
        ));
        // 未知の encoding は構造として拒否する。別 OS の path を解釈しない。
        let unknown = br#"{"encoding":"unix-bytes","units":[1]}"#;
        assert!(serde_json::from_slice::<PathValue>(unknown).is_err());
    }

    #[derive(Default)]
    struct FakeSink {
        writes: Vec<Vec<u8>>,
        fail_next: bool,
    }

    impl SessionSink for FakeSink {
        fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
            if self.fail_next {
                self.fail_next = false;
                return Err("disk full".into());
            }
            self.writes.push(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn the_pending_write_collapses_to_the_latest() {
        let mut writer = OrderedWriter::new(FakeSink::default());
        writer.request(1, b"one".to_vec());
        writer.request(2, b"two".to_vec());
        writer.request(3, b"three".to_vec());
        assert!(writer.is_dirty());

        assert_eq!(writer.flush(), Some(Ok(3)));
        // 待機分は最新だけが書かれる。
        assert_eq!(writer.sink().writes, vec![b"three".to_vec()]);
        assert!(!writer.is_dirty());
        assert_eq!(writer.written_revision(), 3);
    }

    #[test]
    fn an_older_request_does_not_replace_a_newer_one() {
        let mut writer = OrderedWriter::new(FakeSink::default());
        writer.request(5, b"five".to_vec());
        // 遅れて届いた古い要求は捨てる。
        writer.request(4, b"four".to_vec());
        assert_eq!(writer.flush(), Some(Ok(5)));
        assert_eq!(writer.sink().writes, vec![b"five".to_vec()]);

        // 既に書いた版より古い要求も書かない。
        writer.request(4, b"stale".to_vec());
        assert!(!writer.is_dirty());
        assert_eq!(writer.flush(), None);
    }

    #[test]
    fn a_failed_write_keeps_the_request_for_a_retry() {
        let mut writer = OrderedWriter::new(FakeSink::default());
        writer.request(1, b"one".to_vec());
        writer.sink.fail_next = true;

        assert_eq!(writer.flush(), Some(Err("disk full".into())));
        // 失敗を成功扱いにせず、要求も残す。
        assert!(writer.is_dirty());
        assert_eq!(writer.written_revision(), 0);
        assert_eq!(writer.last_failure(), Some("disk full"));

        assert_eq!(writer.flush(), Some(Ok(1)));
        assert_eq!(writer.sink().writes, vec![b"one".to_vec()]);
        assert_eq!(writer.last_failure(), None);
    }

    #[test]
    fn sort_and_column_values_map_to_the_view_model() {
        use crate::sorting::{Column, SortState};
        for state in [
            SortState::None,
            SortState::Asc(Column::Name),
            SortState::Desc(Column::Modified),
        ] {
            let value = SortValue::from(state);
            assert_eq!(SortState::from(value), state);
        }
    }

    #[test]
    fn saving_waits_for_quiet_or_the_cap() {
        // 静まったら書く。
        assert!(should_write(SAVE_DEBOUNCE, Duration::ZERO));
        // 静まっていなくても、最初の変更から上限を過ぎれば書く。
        assert!(should_write(Duration::from_millis(10), SAVE_DEBOUNCE_CAP));
        // どちらも未満なら待つ。
        assert!(!should_write(
            Duration::from_millis(100),
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn a_missing_primary_is_not_restored_from_the_backup() {
        let backup = snapshot(vec![tab(1, r"C:\work")]).to_json().unwrap();
        assert_eq!(resolve_restore(None, Some(&backup)), RestoreOutcome::Absent);
    }

    #[test]
    fn a_corrupt_primary_falls_back_to_a_valid_backup() {
        let primary = b"{ not json";
        let backup = snapshot(vec![tab(4, r"C:\work\alpha")]).to_json().unwrap();
        match resolve_restore(Some(primary), Some(&backup)) {
            RestoreOutcome::Restored(snapshot) => {
                assert_eq!(snapshot.workspaces[0].panes[0].tabs[0].key, 4);
            }
            other => panic!("expected a restored snapshot, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_primary_without_a_backup_stays_unreadable() {
        let outcome = resolve_restore(Some(b"{ not json"), None);
        assert!(
            matches!(outcome, RestoreOutcome::Unreadable(_)),
            "{outcome:?}"
        );
    }

    /// future schema は古い backup へも降りない。降りると新しい保存を潰す。
    #[test]
    fn a_future_schema_is_neither_read_nor_replaced() {
        let mut newer = snapshot(vec![tab(1, r"C:\work")]);
        newer.schema_version = SCHEMA_VERSION + 1;
        let primary = serde_json::to_vec(&newer).unwrap();
        let backup = snapshot(vec![tab(2, r"C:\old")]).to_json().unwrap();

        assert_eq!(
            resolve_restore(Some(&primary), Some(&backup)),
            RestoreOutcome::FutureSchema {
                found: SCHEMA_VERSION + 1
            }
        );
    }
}
