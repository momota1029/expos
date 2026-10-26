//! メニューバーと、ポインタの位置に出る menu。出す項目と disabled は `commands` の表から読む。
//!
//! 表示そのもの（div の組み立て）は `shell::menu_surface` が持つ。ここは「どの面に何を
//! 出すか」と「開いているか」だけを決め、実行は対象の view / shell が行う。filesystem も
//! operation state も知らない。
//!
//! ハンバーガーは一覧ではなく**メニューバー**をトグルする。出ている間だけ
//! `編集 / 表示 / 移動` が並び、タイトルを指す（か触る）とその dropdown が開く。開いた
//! あとは指を移すだけで menu が切り替わる。これは v5 の menu mode と同じ形
//! （`v5/crates/titlebar/src/menu_bar.rs`）。
//!
//! v5 のメニューバーは File / Edit / View / Go / Help の 5 つで、File / Edit / Help は
//! 空だった。v6 は**中身のある menu だけ**を出す。開いても何も無い menu は出さない。

use super::commands::{self, CommandContext};
use super::entry_identity::EntryKey;
use super::input;
use super::shell::{Message, PaneId, Translator, WorkspaceId};
use gpui_filer_input::ActionId;
use std::collections::HashSet;

/// トップレベルの menu。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Menu {
    File,
    Edit,
    View,
    Go,
}

/// 開いている面。メニューバーから出る dropdown か、ポインタの位置に出る popup。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Open {
    /// メニューバーの dropdown。中身はその menu の行だけ。
    Menu(Menu),
    /// ポインタの位置に出る popup。
    Popup(Popup),
}

/// ポインタの位置に出る menu の中身。開いた場所で変わる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Popup {
    /// 項目の上。
    Item,
    /// 余白。
    Background,
    /// ファイルタブ。
    Tab,
    ReopenTabs,
    Workspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ViewSubmenu {
    Layout,
    Columns,
    Theme,
    Language,
    Compression,
    Creation,
}

/// 分岐を実行コマンドと区別し、テーマ名は表示した項目に保持する。
#[derive(Clone, Debug)]
pub(super) enum Row {
    Leaf(Entry),
    Info {
        label: String,
    },
    Theme {
        basename: String,
        label: String,
        checked: bool,
    },
    Branch {
        submenu: ViewSubmenu,
        label: String,
    },
}

impl Row {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Leaf(entry) => &entry.label,
            Self::Info { label } | Self::Theme { label, .. } | Self::Branch { label, .. } => label,
        }
    }

    pub(super) fn checked(&self) -> bool {
        match self {
            Self::Leaf(entry) => entry.checked,
            Self::Theme { checked, .. } => *checked,
            Self::Info { .. } | Self::Branch { .. } => false,
        }
    }
}

/// Build a bounded, descriptive projection of the optional provider snapshot.
pub(super) fn seven_zip_info_label(
    snapshot: &crate::archive_provider::ArchiveProviderSnapshot,
    translator: &Translator,
) -> String {
    use crate::archive_provider::ArchiveProviderState;

    let provider = translator.text(Message::SevenZipProvider).into_owned();
    match &snapshot.state {
        ArchiveProviderState::Unknown => {
            format!("{provider}: {}", translator.text(Message::SevenZipUnknown))
        }
        ArchiveProviderState::Discovering => format!(
            "{provider}: {}",
            translator.text(Message::SevenZipDiscovering)
        ),
        ArchiveProviderState::Unavailable => format!(
            "{provider}: {}",
            translator.text(Message::SevenZipUnavailable)
        ),
        ArchiveProviderState::Available(executable) => {
            let mut label = format!(
                "{provider}: {}",
                translator.text(Message::SevenZipAvailable)
            );
            if let Some(version) = executable.version.as_deref() {
                let version = bounded_text(version, 32);
                if !version.is_empty() {
                    label.push_str(" ( ");
                    label.push_str(&version);
                    label.push_str(" )");
                }
            }
            if let Some(formats) = executable.formats.as_ref() {
                let names = formats
                    .iter()
                    .take(6)
                    .map(|format| {
                        let name = bounded_text(&format.name, 24);
                        if !name.is_empty() && format.supports_7z_creation() {
                            format!(
                                "{name} ({})",
                                translator.text(Message::SevenZipFormatCreate)
                            )
                        } else {
                            name
                        }
                    })
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>();
                if !names.is_empty() {
                    label.push_str(" — ");
                    label.push_str(&names.join(", "));
                    if formats.len() > names.len() {
                        label.push_str(", …");
                    }
                }
            }
            label
        }
    }
}

pub(super) fn append_seven_zip_info_row(
    rows: &mut Vec<Row>,
    snapshot: &crate::archive_provider::ArchiveProviderSnapshot,
    translator: &Translator,
) {
    rows.push(Row::Info {
        label: seven_zip_info_label(snapshot, translator),
    });
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

pub(super) const TAB_CLOSE: &str = "tab.close";
pub(super) const TAB_DUPLICATE: &str = "tab.duplicate";
pub(super) const TAB_CLOSE_OTHERS: &str = "tab.close_others";
pub(super) const TAB_CLOSE_TO_RIGHT: &str = "tab.close_to_right";
pub(super) const TAB_TOGGLE_LOCK: &str = "tab.toggle_lock";
pub(super) const WORKSPACE_DUPLICATE: &str = "workspace.duplicate";
pub(super) const REOPEN_TAB: &str = "tab.reopen";
pub(super) const OPEN_IN_NEW_PANE: &str = "folder.open_in_new_pane";
pub(super) const CREATE_ZIP: &str = "file.create_zip";
pub(super) const CREATE_SHORTCUT: &str = "file.create_shortcut";
pub(super) const CREATE_7Z: &str = "file.create_7z";
pub(super) const EXTRACT_TO_FOLDER: &str = "file.extract_to_folder";
pub(super) const OPEN_IN_TERMINAL: &str = "folder.open_in_windows_terminal";
pub(super) const NEW_WINDOW: &str = "window.new";
pub(super) const LANGUAGE_SYSTEM: &str = "language.system";
pub(super) const LANGUAGE_JAPANESE: &str = "language.japanese";
pub(super) const LANGUAGE_ENGLISH: &str = "language.english";
#[cfg(debug_assertions)]
pub(super) const LANGUAGE_PSEUDO: &str = "language.pseudo";

#[derive(Clone)]
pub(super) struct TabMenuTarget {
    pub(super) workspace: WorkspaceId,
    pub(super) pane: PaneId,
    pub(super) id: usize,
    /// 開いた menu が指していた並びと lock 状態。位置依存の bulk action は完全一致を要求する。
    pub(super) snapshot: Vec<(usize, bool)>,
}

/// 項目の並び。Edit の選択解除、View の表示形式、Go の履歴。
const EDIT: &[ActionId] = &[
    ActionId::new(input::EXIT_SELECTION),
    ActionId::new(input::SELECT_EXTENSION_ON_RENAME),
    ActionId::new(input::RELOAD_KEYMAP),
];
const FILE: &[ActionId] = &[
    ActionId::new(NEW_WINDOW),
    ActionId::new(input::CREATE_FOLDER),
];
const VIEW: &[ActionId] = &[
    ActionId::new(input::LAYOUT_ROWS),
    ActionId::new(input::LAYOUT_TILES),
    ActionId::new(input::LAYOUT_ICONS),
    ActionId::new(input::TOGGLE_COLUMN_KIND),
    ActionId::new(input::TOGGLE_COLUMN_SIZE),
    ActionId::new(input::TOGGLE_COLUMN_MODIFIED),
    ActionId::new(input::MOVE_NAME_LEFT),
    ActionId::new(input::MOVE_NAME_RIGHT),
    ActionId::new(input::MOVE_KIND_LEFT),
    ActionId::new(input::MOVE_KIND_RIGHT),
    ActionId::new(input::MOVE_SIZE_LEFT),
    ActionId::new(input::MOVE_SIZE_RIGHT),
    ActionId::new(input::MOVE_MODIFIED_LEFT),
    ActionId::new(input::MOVE_MODIFIED_RIGHT),
    ActionId::new(input::TOGGLE_FILE_INFO_PANE),
];
const GO: &[ActionId] = &[
    ActionId::new(input::NAVIGATE_BACK),
    ActionId::new(input::NAVIGATE_FORWARD),
];
/// 項目の popup。ファイル操作が入るまでは開くことだけ。
const POPUP_ITEM: &[ActionId] = &[
    ActionId::new(input::ACTIVATE),
    ActionId::new(input::ACTIVATE_NEW_TAB),
    ActionId::new(input::COPY_PATHS),
    ActionId::new(input::COPY_NAMES),
    ActionId::new(input::RENAME),
    ActionId::new(input::REFRESH),
];
/// 余白の popup。
const POPUP_BACKGROUND: &[ActionId] = &[
    ActionId::new(input::NAVIGATE_BACK),
    ActionId::new(input::NAVIGATE_FORWARD),
    ActionId::new(input::CREATE_FILE),
    ActionId::new(input::CREATE_FOLDER),
    ActionId::new(input::COPY_CURRENT_PATH),
    ActionId::new(input::PASTE_LINK),
    ActionId::new(input::PASTE_JUNCTION),
    ActionId::new(input::REFRESH),
    ActionId::new(input::EXIT_SELECTION),
];

impl Menu {
    /// 左から並べる順。
    pub(super) const ALL: &'static [Self] = &[Self::File, Self::Edit, Self::View, Self::Go];

    pub(super) fn label(self, translator: &Translator) -> String {
        translator
            .text(match self {
                Self::File => Message::MenuFile,
                Self::Edit => Message::MenuEdit,
                Self::View => Message::MenuView,
                Self::Go => Message::MenuGo,
            })
            .into_owned()
    }

    /// この menu に出す action。表 (`commands`) の action を並べるだけ。
    pub(super) fn entries(self) -> &'static [ActionId] {
        match self {
            Self::File => FILE,
            Self::Edit => EDIT,
            Self::View => VIEW,
            Self::Go => GO,
        }
    }
}

impl Popup {
    /// この面に出す action。
    fn entries(self) -> &'static [ActionId] {
        match self {
            Self::Item => POPUP_ITEM,
            Self::Background => POPUP_BACKGROUND,
            Self::Tab | Self::ReopenTabs | Self::Workspace => &[],
        }
    }
}

/// メニューに出す 1 行。
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) action: ActionId,
    pub(super) label: String,
    /// 今の状態で実行できるか。key の消費と同じ判定を使う。
    pub(super) enabled: bool,
    pub(super) checked: bool,
}

/// 開いている面の行を組み立てる。
///
/// 見出しは無い。メニューバーの dropdown はバーのタイトルが名前を出しており、popup は
/// 開いた場所が名前の代わりになる。
///
/// `has_item` は「対象になる項目があるか」。popup は開いた時に捕まえた項目、
/// メニューバーの dropdown は focus している項目で決まる。
pub(super) fn rows(
    open: Open,
    context: &CommandContext,
    has_item: bool,
    translator: &Translator,
    select_extension_on_rename: bool,
) -> Vec<Entry> {
    let actions = match open {
        Open::Menu(menu) => menu.entries(),
        Open::Popup(popup) => popup.entries(),
    };
    build_rows(
        actions,
        context,
        has_item,
        translator,
        select_extension_on_rename,
    )
}

fn build_rows(
    actions: &[ActionId],
    context: &CommandContext,
    has_item: bool,
    translator: &Translator,
    select_extension_on_rename: bool,
) -> Vec<Entry> {
    actions
        .iter()
        .filter_map(|action| {
            // ラベルの無い action は menu に出さない。表に足しただけで menu の項目に
            // したくないもの（pick など）はここで落ちる。
            let label = label(*action, translator)?;
            Some(Entry {
                action: *action,
                label,
                enabled: action.as_str() == NEW_WINDOW
                    || commands::available(*action, context, has_item),
                checked: action.as_str() == input::SELECT_EXTENSION_ON_RENAME
                    && select_extension_on_rename,
            })
        })
        .collect()
}

/// action のラベル。menu に出さない action は `None`。
///
/// action ID を label にしない。表示文字列は i18n が持ち、ここは対応だけを決める。
pub(super) fn label(action: ActionId, translator: &Translator) -> Option<String> {
    let message = match action.as_str() {
        NEW_WINDOW => Message::NewWindow,
        input::EXIT_SELECTION => Message::ClearSelection,
        input::REFRESH => Message::Refresh,
        input::NAVIGATE_BACK => Message::NavigateBack,
        input::NAVIGATE_FORWARD => Message::NavigateForward,
        input::LAYOUT_ROWS => Message::LayoutRows,
        input::LAYOUT_TILES => Message::LayoutTiles,
        input::LAYOUT_ICONS => Message::LayoutIcons,
        input::TOGGLE_COLUMN_KIND => Message::ToggleColumnKind,
        input::TOGGLE_COLUMN_SIZE => Message::ToggleColumnSize,
        input::TOGGLE_COLUMN_MODIFIED => Message::ToggleColumnModified,
        input::MOVE_NAME_LEFT => Message::MoveNameLeft,
        input::MOVE_NAME_RIGHT => Message::MoveNameRight,
        input::MOVE_KIND_LEFT => Message::MoveKindLeft,
        input::MOVE_KIND_RIGHT => Message::MoveKindRight,
        input::MOVE_SIZE_LEFT => Message::MoveSizeLeft,
        input::MOVE_SIZE_RIGHT => Message::MoveSizeRight,
        input::MOVE_MODIFIED_LEFT => Message::MoveModifiedLeft,
        input::MOVE_MODIFIED_RIGHT => Message::MoveModifiedRight,
        input::TOGGLE_FILE_INFO_PANE => Message::ToggleFileInfoPane,
        input::ACTIVATE => Message::Activate,
        input::ACTIVATE_NEW_TAB => Message::ActivateNewTab,
        input::COPY_PATHS => Message::CopyPaths,
        input::COPY_CURRENT_PATH => Message::CopyPaths,
        input::COPY_NAMES => Message::CopyNames,
        input::RENAME => Message::Rename,
        input::PROPERTIES => Message::Properties,
        CREATE_ZIP => Message::CreateZip,
        CREATE_SHORTCUT => Message::CreateShortcut,
        CREATE_7Z => Message::Create7z,
        EXTRACT_TO_FOLDER => Message::ExtractToFolder,
        OPEN_IN_TERMINAL => Message::OpenInWindowsTerminal,
        input::PASTE_LINK => Message::PasteLinkHere,
        input::PASTE_JUNCTION => Message::PasteJunctionHere,
        input::CREATE_FOLDER => Message::CreateFolder,
        input::CREATE_FILE => Message::CreateFile,
        input::SELECT_EXTENSION_ON_RENAME => Message::SelectExtensionOnRename,
        input::RELOAD_KEYMAP => Message::ReloadKeymap,
        _ => return None,
    };
    Some(translator.text(message).into_owned())
}

/// メニューバーと、選んでいる行。
///
/// ハンバーガーが `bar` をトグルし、出ている間だけタイトルを並べる。面は同時に一つで、
/// dropdown か popup のどちらか。
#[derive(Default)]
pub(super) struct MenuBar {
    /// メニューバーを出しているか。
    bar: bool,
    open: Option<Open>,
    /// 選んでいる行。マウスの hover でもキーの上下でも、ここだけが動く。
    highlight: usize,
    keyboard_navigation_started: bool,
    mouse_hover: Option<usize>,
    view_submenu: Option<ViewSubmenu>,
    parent_highlight: usize,
}

impl MenuBar {
    /// メニューバーが出ているか。key を menu が引き受けるかの判定にも使う。
    pub(super) fn bar_visible(&self) -> bool {
        self.bar
    }

    pub(super) fn open(&self) -> Option<Open> {
        self.open
    }

    pub(super) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// 開いている dropdown の menu。popup のときは `None`。
    pub(super) fn open_menu(&self) -> Option<Menu> {
        match self.open {
            Some(Open::Menu(menu)) => Some(menu),
            _ => None,
        }
    }

    /// ハンバーガー。出ていれば menu モードごと閉じ、出ていなければバーだけ出す。
    pub(super) fn toggle_bar(&mut self) {
        if self.bar {
            self.close();
            return;
        }
        self.bar = true;
        self.open = None;
        self.highlight = 0;
        self.keyboard_navigation_started = false;
        self.mouse_hover = None;
        self.view_submenu = None;
        self.parent_highlight = 0;
    }

    /// menu の dropdown を開く。タイトルの hover と click の両方から呼ぶ。
    ///
    /// **dropdown はバーの中にある。** 開いている間はバーも出ている、という不変条件を
    /// ここで保つ（描画は「バーが出ているか」と「どの面が開いているか」を別に見る）。
    ///
    /// 同じ menu なら選択を動かさない。hover は dropdown へ指を下ろす途中でも切れるので、
    /// 毎回先頭へ戻すと、選んだ行が指の下から逃げてしまう。
    pub(super) fn show_menu(&mut self, menu: Menu) {
        if self.open != Some(Open::Menu(menu)) {
            self.highlight = 0;
            self.keyboard_navigation_started = false;
            self.view_submenu = None;
            self.parent_highlight = 0;
        }
        self.bar = true;
        self.open = Some(Open::Menu(menu));
    }

    /// ポインタの位置に menu を出す。面は同時に一つなのでメニューバーは閉じる。
    pub(super) fn open_popup(&mut self, popup: Popup) {
        self.bar = false;
        self.open = Some(Open::Popup(popup));
        self.highlight = 0;
        self.keyboard_navigation_started = false;
        self.mouse_hover = None;
        self.view_submenu = None;
        self.parent_highlight = 0;
    }

    /// メニューモードごと閉じる。ハンバーガー・外側のクリック・Escape がここへ来る。
    pub(super) fn close(&mut self) {
        self.bar = false;
        self.open = None;
        self.highlight = 0;
        self.keyboard_navigation_started = false;
        self.mouse_hover = None;
        self.view_submenu = None;
        self.parent_highlight = 0;
    }

    /// 選んでいる行を動かす。端では止める。
    pub(super) fn move_highlight(&mut self, offset: isize, rows: usize) {
        let active = self.active_highlight();
        self.mouse_hover = None;
        self.keyboard_navigation_started = true;
        if rows == 0 {
            self.highlight = 0;
            return;
        }
        if active.is_none() {
            self.highlight = if offset < 0 { rows - 1 } else { 0 };
            return;
        }
        let last = (rows - 1) as isize;
        let next = (active.unwrap().min(rows - 1) as isize + offset).clamp(0, last);
        self.highlight = next as usize;
    }

    /// 指した行を選ぶ。マウスの hover から呼ぶ。
    #[cfg(test)]
    pub(super) fn highlight(&mut self, index: usize) {
        self.mouse_hover = None;
        self.highlight = index;
        self.keyboard_navigation_started = true;
    }

    pub(super) fn hover(&mut self, index: usize) {
        self.mouse_hover = Some(index);
    }

    pub(super) fn clear_hover(&mut self, index: usize) -> bool {
        if self.mouse_hover == Some(index) {
            self.mouse_hover = None;
            true
        } else {
            false
        }
    }

    pub(super) fn highlight_index(&self) -> usize {
        self.mouse_hover.unwrap_or(self.highlight)
    }

    pub(super) fn active_highlight(&self) -> Option<usize> {
        self.mouse_hover
            .or_else(|| self.keyboard_navigation_started.then_some(self.highlight))
    }

    /// キー操作で submenu に入ったときは先頭 child row を keyboard selection にする。
    pub(super) fn activate_first_row(&mut self) {
        self.highlight = 0;
        self.mouse_hover = None;
        self.keyboard_navigation_started = true;
    }

    pub(super) fn view_submenu(&self) -> Option<ViewSubmenu> {
        self.view_submenu
    }

    pub(super) fn parent_highlight_index(&self) -> usize {
        self.parent_highlight
    }

    pub(super) fn open_submenu(&mut self, submenu: ViewSubmenu, parent_index: usize) {
        if self.view_submenu != Some(submenu) {
            self.highlight = 0;
            self.keyboard_navigation_started = false;
        }
        self.parent_highlight = parent_index;
        self.mouse_hover = None;
        self.view_submenu = Some(submenu);
    }

    pub(super) fn close_submenu(&mut self) -> bool {
        if self.view_submenu.take().is_none() {
            return false;
        }
        self.highlight = self.parent_highlight;
        self.mouse_hover = None;
        self.keyboard_navigation_started = true;
        true
    }

    pub(super) fn reset_view_submenu(&mut self) {
        self.view_submenu = None;
        self.parent_highlight = 0;
        self.highlight = 0;
        self.mouse_hover = None;
        self.keyboard_navigation_started = false;
    }

    /// 選んでいる行。使えるかどうかは呼び出し側が見る。
    pub(super) fn highlighted<'a, T>(&self, rows: &'a [T]) -> Option<&'a T> {
        rows.get(self.active_highlight()?)
    }
}

/// menu を開いた時に捕捉する対象。
///
/// 選択が変わった後に表示中の menu から実行されたとき、別のタブへ黙って配送しないため。
/// 開いた時の pane / tab / view identity と view の世代、選択 identity を持ち、
/// 実行の直前に照合する。項目を
/// 対象にする menu は、index ではなく **key** を持つ。並びが変わっても同じ項目に当たる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MenuTarget {
    pub(super) workspace: WorkspaceId,
    pub(super) pane: PaneId,
    pub(super) tab: usize,
    pub(super) view: Option<gpui::EntityId>,
    pub(super) generation: u64,
    pub(super) presentation_revision: u64,
    pub(super) item: Option<EntryKey>,
    /// FileView が右クリック時に確定した表示順の選択キー。
    pub(super) selected_keys: Vec<EntryKey>,
    /// popup の操作対象。選択済み複数項目では全選択、その他は右クリック項目だけ。
    pub(super) context_keys: Vec<EntryKey>,
    /// context target が選択由来なら、実行時にも同じ selection を要求する。
    pub(super) selection_derived: bool,
    /// この popup 対象が単一の通常 filesystem entry として Rename 可能か。
    pub(super) rename_eligible: bool,
    pub(super) paste_target: Option<crate::InternalDropTarget>,
    pub(super) clipboard_has_files: bool,
    pub(super) background_destination: Option<std::path::PathBuf>,
    pub(super) background_destination_identity:
        Option<crate::filesystem_identity::FilesystemIdentity>,
    pub(super) touch_target: bool,
    /// Immutable optional creator captured when the popup opens.
    pub(super) seven_zip_creation: Option<SevenZipCreationHint>,
    /// Immutable read-capable provider captured separately from creator support.
    pub(super) seven_zip_read: Option<SevenZipReadHint>,
    /// Ordered ordinary filesystem source identities captured with the item popup.
    pub(super) seven_zip_source_identities: Vec<crate::filesystem_identity::FilesystemIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SevenZipCreationHint {
    pub(super) revision: u64,
    pub(super) binding: crate::archive_provider::ArchiveOperationBinding,
}

impl SevenZipCreationHint {
    pub(super) fn capture(
        snapshot: &crate::archive_provider::ArchiveProviderSnapshot,
    ) -> Option<Self> {
        use crate::archive_provider::ArchiveProviderState;
        let ArchiveProviderState::Available(executable) = &snapshot.state else {
            return None;
        };
        if !executable
            .formats
            .as_ref()?
            .iter()
            .any(|format| format.supports_7z_creation())
        {
            return None;
        }
        Some(Self {
            revision: snapshot.revision,
            binding: executable.execution_hints.operation_binding.clone()?,
        })
    }

    pub(super) fn matches_live(
        &self,
        snapshot: &crate::archive_provider::ArchiveProviderSnapshot,
    ) -> bool {
        Self::capture(snapshot).as_ref() == Some(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SevenZipReadHint {
    pub(super) revision: u64,
    pub(super) binding: crate::archive_provider::ArchiveOperationBinding,
}

impl SevenZipReadHint {
    pub(super) fn capture(
        snapshot: &crate::archive_provider::ArchiveProviderSnapshot,
    ) -> Option<Self> {
        let (_, binding) = snapshot.seven_zip_read_binding()?;
        Some(Self {
            revision: snapshot.revision,
            binding: binding.clone(),
        })
    }

    pub(super) fn matches_live(
        &self,
        snapshot: &crate::archive_provider::ArchiveProviderSnapshot,
    ) -> bool {
        Self::capture(snapshot).as_ref() == Some(self)
    }
}

/// FileView から Shell へ渡す、右クリック時点の immutable menu snapshot。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ItemMenuRequest {
    pub(super) clicked: Option<EntryKey>,
    pub(super) selected_keys: Vec<EntryKey>,
    pub(super) context_keys: Vec<EntryKey>,
    pub(super) selection_derived: bool,
    pub(super) rename_eligible: bool,
    pub(super) paste_target: Option<crate::InternalDropTarget>,
    pub(super) background_destination: Option<std::path::PathBuf>,
    pub(super) background_destination_identity:
        Option<crate::filesystem_identity::FilesystemIdentity>,
    pub(super) touch_target: bool,
    pub(super) generation: u64,
    pub(super) presentation_revision: u64,
}

pub(super) fn selection_matches(captured: &HashSet<EntryKey>, current: &HashSet<EntryKey>) -> bool {
    captured == current
}

pub(super) fn resolve_item_index<'a>(
    mut keys: impl Iterator<Item = &'a EntryKey>,
    target: &EntryKey,
) -> Option<usize> {
    keys.position(|key| key == target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ViewLayout;

    fn provider_snapshot(
        state: crate::archive_provider::ArchiveProviderState,
    ) -> crate::archive_provider::ArchiveProviderSnapshot {
        crate::archive_provider::ArchiveProviderSnapshot { revision: 1, state }
    }

    fn ready_provider() -> crate::archive_provider::ArchiveProviderSnapshot {
        use crate::archive_provider::*;
        let hint = |path: &str, file_index| ArchiveFileHint {
            path: path.into(),
            identity: ArchiveFileIdentity {
                volume_serial: 1,
                file_index,
            },
        };
        ArchiveProviderSnapshot {
            revision: 7,
            state: ArchiveProviderState::Available(SevenZipExecutable {
                path: "7z.exe".into(),
                execution_hints: ArchiveExecutionHints {
                    operation_binding: Some(ArchiveOperationBinding {
                        executable: hint("7z.exe", 2),
                        zip_dll: hint("7z.dll", 3),
                    }),
                    ..Default::default()
                },
                version: None,
                formats: Some(vec![SevenZipFormat {
                    name: "7z".into(),
                    flags: "C........".into(),
                }]),
            }),
        }
    }

    #[test]
    fn seven_zip_hint_requires_available_exact_creator_and_same_live_capture() {
        use crate::archive_provider::{ArchiveProviderState, SevenZipFormat};
        let ready = ready_provider();
        let captured = SevenZipCreationHint::capture(&ready).unwrap();
        assert!(captured.matches_live(&ready));
        assert!(
            SevenZipCreationHint::capture(&provider_snapshot(ArchiveProviderState::Unknown))
                .is_none()
        );
        assert!(
            SevenZipCreationHint::capture(&provider_snapshot(ArchiveProviderState::Discovering))
                .is_none()
        );
        assert!(
            SevenZipCreationHint::capture(&provider_snapshot(ArchiveProviderState::Unavailable))
                .is_none()
        );

        let mut changed = ready.clone();
        changed.revision += 1;
        assert!(!captured.matches_live(&changed));
        let mut wrong_binding = ready.clone();
        if let ArchiveProviderState::Available(executable) = &mut wrong_binding.state {
            executable
                .execution_hints
                .operation_binding
                .as_mut()
                .unwrap()
                .zip_dll
                .identity
                .file_index += 1;
        }
        assert!(!captured.matches_live(&wrong_binding));
        for (name, flags) in [("zip", "C........"), ("7z", "c........")] {
            let mut unsupported = ready.clone();
            if let ArchiveProviderState::Available(executable) = &mut unsupported.state {
                executable.formats = Some(vec![SevenZipFormat {
                    name: name.into(),
                    flags: flags.into(),
                }]);
            }
            assert!(SevenZipCreationHint::capture(&unsupported).is_none());
        }
        let mut missing = ready;
        if let ArchiveProviderState::Available(executable) = &mut missing.state {
            executable.execution_hints.operation_binding = None;
        }
        assert!(SevenZipCreationHint::capture(&missing).is_none());
        let mut missing_formats = ready_provider();
        if let ArchiveProviderState::Available(executable) = &mut missing_formats.state {
            executable.formats = None;
        }
        assert!(SevenZipCreationHint::capture(&missing_formats).is_none());
        assert_eq!(
            label(
                ActionId::new(CREATE_7Z),
                &Translator::new(crate::shell::Locale::Japanese)
            )
            .as_deref(),
            Some("7z に圧縮")
        );
        assert_eq!(
            label(
                ActionId::new(CREATE_7Z),
                &Translator::new(crate::shell::Locale::English)
            )
            .as_deref(),
            Some("Compress to 7z")
        );
    }

    #[test]
    fn seven_zip_read_hint_needs_readable_binding_but_not_creation_support() {
        use crate::archive_provider::{ArchiveProviderState, SevenZipFormat};
        let mut read_only = ready_provider();
        if let ArchiveProviderState::Available(executable) = &mut read_only.state {
            executable.formats = Some(vec![SevenZipFormat {
                name: "7z".into(),
                flags: "........".into(),
            }]);
        }
        let captured = SevenZipReadHint::capture(&read_only).unwrap();
        assert!(captured.matches_live(&read_only));
        assert!(SevenZipCreationHint::capture(&read_only).is_none());

        for formats in [
            vec![SevenZipFormat {
                name: "7z".into(),
                flags: ".........!".into(),
            }],
            vec![
                SevenZipFormat {
                    name: "7z".into(),
                    flags: ".........".into(),
                },
                SevenZipFormat {
                    name: "7z".into(),
                    flags: ".........".into(),
                },
            ],
        ] {
            let mut invalid = read_only.clone();
            if let ArchiveProviderState::Available(executable) = &mut invalid.state {
                executable.formats = Some(formats);
            }
            assert!(SevenZipReadHint::capture(&invalid).is_none());
        }

        let mut changed = read_only.clone();
        changed.revision += 1;
        assert!(!captured.matches_live(&changed));
        let mut changed_binding = read_only.clone();
        if let ArchiveProviderState::Available(executable) = &mut changed_binding.state {
            executable
                .execution_hints
                .operation_binding
                .as_mut()
                .unwrap()
                .zip_dll
                .identity
                .file_index += 1;
        }
        assert!(!captured.matches_live(&changed_binding));
        for state in [
            ArchiveProviderState::Unknown,
            ArchiveProviderState::Discovering,
            ArchiveProviderState::Unavailable,
        ] {
            assert!(SevenZipReadHint::capture(&provider_snapshot(state)).is_none());
        }
        if let ArchiveProviderState::Available(executable) = &mut read_only.state {
            executable.execution_hints.operation_binding = None;
        }
        assert!(SevenZipReadHint::capture(&read_only).is_none());
        assert!(
            SevenZipReadHint::capture(&provider_snapshot(ArchiveProviderState::Unknown)).is_none()
        );
    }

    #[test]
    fn seven_zip_info_projects_every_discovery_state_and_bounded_details() {
        use crate::archive_provider::{ArchiveProviderState, SevenZipExecutable, SevenZipFormat};

        let translator = translator();
        assert_eq!(
            seven_zip_info_label(
                &provider_snapshot(ArchiveProviderState::Unknown),
                &translator
            ),
            "7-Zip: 状態不明"
        );
        assert_eq!(
            seven_zip_info_label(
                &provider_snapshot(ArchiveProviderState::Discovering),
                &translator
            ),
            "7-Zip: 確認中…"
        );
        assert_eq!(
            seven_zip_info_label(
                &provider_snapshot(ArchiveProviderState::Unavailable),
                &translator
            ),
            "7-Zip: 利用できません"
        );

        let executable = SevenZipExecutable {
            path: std::path::PathBuf::from("ignored-path"),
            execution_hints: Default::default(),
            version: Some("24.09".into()),
            formats: Some(
                ["7z", "zip", "rar", "tar", "gzip", "xz", "zstd"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| SevenZipFormat {
                        name: name.into(),
                        flags: if index == 0 {
                            "C........".into()
                        } else {
                            "ignored-flags".into()
                        },
                    })
                    .collect(),
            ),
        };
        let label = seven_zip_info_label(
            &provider_snapshot(ArchiveProviderState::Available(executable)),
            &translator,
        );
        assert_eq!(
            label,
            "7-Zip: 利用可能 ( 24.09 ) — 7z (作成対応), zip, rar, tar, gzip, xz, …"
        );
        assert!(!label.contains("ignored"));
    }

    #[test]
    fn file_menu_info_row_keeps_command_positions_and_is_keyboard_selectable_noop() {
        use crate::archive_provider::ArchiveProviderState;

        let translator = translator();
        let context = context();
        let entries = rows(Open::Menu(Menu::File), &context, false, &translator, false);
        let mut projected = entries.iter().cloned().map(Row::Leaf).collect::<Vec<_>>();
        let command_count = projected.len();
        append_seven_zip_info_row(
            &mut projected,
            &provider_snapshot(ArchiveProviderState::Unknown),
            &translator,
        );
        assert_eq!(projected.len(), command_count + 1);
        for (row, entry) in projected.iter().zip(entries.iter()) {
            match row {
                Row::Leaf(actual) => assert_eq!(actual.action, entry.action),
                _ => panic!("the informational row must follow all commands"),
            }
        }
        assert!(matches!(projected.last(), Some(Row::Info { .. })));

        let mut bar = MenuBar::default();
        bar.toggle_bar();
        bar.show_menu(Menu::File);
        bar.move_highlight(command_count as isize, projected.len());
        assert!(matches!(
            bar.highlighted(&projected),
            Some(Row::Info { .. })
        ));
        // The row has no ActionId, and the keyboard dispatcher deliberately does
        // nothing for Info, leaving the menu open.
        assert_eq!(bar.open(), Some(Open::Menu(Menu::File)));
    }

    fn translator() -> Translator {
        Translator::new(super::super::shell::Locale::Japanese)
    }

    fn context() -> CommandContext {
        CommandContext {
            items: 10,
            location: true,
            ..Default::default()
        }
    }

    #[test]
    fn captured_selection_must_match_current_identity_set() {
        let a = EntryKey::demo(1);
        let b = EntryKey::demo(2);
        let captured = HashSet::from([a.clone()]);

        assert!(!selection_matches(&captured, &HashSet::from([b])));
        assert!(selection_matches(&captured, &HashSet::from([a])));
    }

    #[test]
    fn item_target_follows_key_through_reordering_but_not_replacement() {
        let a = EntryKey::demo(1);
        let b = EntryKey::demo(2);
        let c = EntryKey::demo(3);

        let reordered = [b.clone(), a.clone()];
        let replaced = [b, c];
        assert_eq!(resolve_item_index(reordered.iter(), &a), Some(1));
        assert_eq!(resolve_item_index(replaced.iter(), &a), None);
    }

    /// 出す menu はどれも中身があること。開いて何も無い menu を出さない。
    #[test]
    fn every_menu_has_entries() {
        for menu in Menu::ALL {
            assert!(!menu.entries().is_empty(), "{menu:?}");
        }
    }

    /// menu の見出しは重複しないこと。
    #[test]
    fn the_menu_titles_are_unique() {
        let translator = translator();
        let mut labels = Menu::ALL
            .iter()
            .map(|menu| menu.label(&translator))
            .collect::<Vec<_>>();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), Menu::ALL.len());
    }

    /// 行のラベルは i18n から来る。action ID をそのまま出さない。
    #[test]
    fn entries_carry_translated_labels() {
        let translator = translator();
        let view = rows(
            Open::Menu(Menu::View),
            &context(),
            false,
            &translator,
            false,
        );
        assert_eq!(view.len(), Menu::View.entries().len());
        assert_eq!(view[0].label, "行");
        assert_eq!(view[1].label, "タイル");
        assert_eq!(view[2].label, "アイコン");
        assert_eq!(view[3].label, "種類");
        assert_eq!(view[4].label, "サイズ");
        assert_eq!(view[5].label, "更新日時");
        assert_eq!(view.last().unwrap().label, "ファイル情報");
        assert_eq!(view[0].action, ActionId::new(input::LAYOUT_ROWS));
        assert_eq!(
            view.last().unwrap().action,
            ActionId::new(input::TOGGLE_FILE_INFO_PANE)
        );
    }

    /// dropdown の中身はその menu の行だけ。バーが名前を出すので見出しは置かない。
    #[test]
    fn a_menu_dropdown_holds_only_its_own_rows() {
        let translator = translator();
        let context = context();
        for menu in Menu::ALL {
            let rows = rows(Open::Menu(*menu), &context, false, &translator, false);
            assert_eq!(
                rows.len(),
                menu.entries().len(),
                "{menu:?} は自分の行だけを出す"
            );
        }
    }

    /// disabled は availability と同じ判定。key の消費とずれない。
    #[test]
    fn entries_follow_availability() {
        let translator = translator();
        let mut context = context();

        // 選択が無ければ「選択解除」は押せない。
        let edit = rows(Open::Menu(Menu::Edit), &context, false, &translator, false);
        assert_eq!(edit.len(), 3);
        assert!(!edit[0].enabled);
        assert!(edit[1].enabled);
        assert!(!edit[1].checked);

        context.selection = 1;
        let edit = rows(Open::Menu(Menu::Edit), &context, false, &translator, false);
        assert!(edit[0].enabled);

        let edit = rows(Open::Menu(Menu::Edit), &context, false, &translator, true);
        assert!(edit[1].checked);

        // 履歴の端では「戻る」だけが押せない。
        context.can_back = true;
        let go = rows(Open::Menu(Menu::Go), &context, false, &translator, false);
        assert!(go[0].enabled, "back");
        assert!(!go[1].enabled, "forward");
    }

    /// 表示形式の選択は、表の intent と同じ値を使う。
    #[test]
    fn the_view_entries_are_the_layout_commands() {
        for (action, layout) in [
            (input::LAYOUT_ROWS, ViewLayout::Rows),
            (input::LAYOUT_TILES, ViewLayout::Tiles),
            (input::LAYOUT_ICONS, ViewLayout::Icons),
        ] {
            let intent = commands::resolve(ActionId::new(action), &context(), false);
            assert_eq!(
                intent,
                Some(super::super::commands::AppIntent::SetLayout { layout })
            );
        }
    }

    /// ラベルの無い action は menu に出さない。
    #[test]
    fn actions_without_a_label_stay_out_of_the_menu() {
        let translator = translator();
        // pick は menu の項目にしない（マウスのクリックが持つ動作）。
        assert!(label(ActionId::new(input::PICK), &translator).is_none());
        assert!(label(ActionId::new(input::PICK_TOGGLE), &translator).is_none());
        assert!(label(ActionId::new("nope"), &translator).is_none());
        // 項目の popup が使う action にはラベルがある。
        assert_eq!(
            label(ActionId::new(input::ACTIVATE), &translator).as_deref(),
            Some("開く")
        );
        assert_eq!(
            label(ActionId::new(input::ACTIVATE_NEW_TAB), &translator).as_deref(),
            Some("新しいタブで開く")
        );
        assert_eq!(
            label(ActionId::new(input::RELOAD_KEYMAP), &translator).as_deref(),
            Some("キーボード割り当てを再読み込み")
        );
        assert_eq!(
            label(ActionId::new(CREATE_ZIP), &translator).as_deref(),
            Some("ZIP に圧縮")
        );
        assert_eq!(
            label(ActionId::new(CREATE_SHORTCUT), &translator).as_deref(),
            Some("ショートカットを作成")
        );
    }

    /// ハンバーガーはバーごとトグルする。面は同時に一つ。
    #[test]
    fn the_hamburger_toggles_the_bar() {
        let mut bar = MenuBar::default();
        assert!(!bar.bar_visible());
        assert!(!bar.is_open());

        bar.toggle_bar();
        assert!(bar.bar_visible(), "バーは出る");
        assert!(!bar.is_open(), "dropdown はまだ開かない");

        bar.show_menu(Menu::View);
        assert_eq!(bar.open(), Some(Open::Menu(Menu::View)));
        assert_eq!(bar.open_menu(), Some(Menu::View));

        bar.toggle_bar();
        assert!(!bar.bar_visible(), "ハンバーガーは dropdown ごと閉じる");
        assert!(!bar.is_open());

        // 面は同時に一つ。popup を開けばメニューバーは引く。
        bar.toggle_bar();
        bar.show_menu(Menu::Edit);
        bar.open_popup(Popup::Item);
        assert_eq!(bar.open(), Some(Open::Popup(Popup::Item)));
        assert!(!bar.bar_visible());
        assert_eq!(bar.open_menu(), None);
    }

    /// 同じ menu への hover では選択を動かさない。別の menu へ移ったら先頭から。
    #[test]
    fn moving_along_the_bar_restarts_the_highlight() {
        let mut bar = MenuBar::default();
        bar.toggle_bar();
        bar.show_menu(Menu::Edit);
        bar.move_highlight(1, 3);
        assert_eq!(bar.highlight_index(), 1);

        bar.show_menu(Menu::Edit);
        assert_eq!(bar.highlight_index(), 1, "同じ menu なら選択はそのまま");

        bar.show_menu(Menu::View);
        assert_eq!(bar.highlight_index(), 0, "別の menu は先頭から");
        assert_eq!(bar.open_menu(), Some(Menu::View));
    }

    /// 行の選択は両端で止まる。数が変わっても範囲外にならない。
    #[test]
    fn the_highlight_stays_inside_the_rows() {
        let mut bar = MenuBar::default();
        assert_eq!(bar.highlight_index(), 0);

        bar.move_highlight(1, 3);
        assert_eq!(bar.highlight_index(), 1);
        bar.move_highlight(-1, 3);
        assert_eq!(bar.highlight_index(), 0);
        bar.move_highlight(-1, 3);
        assert_eq!(bar.highlight_index(), 0, "先頭で止まる");
        bar.move_highlight(9, 3);
        assert_eq!(bar.highlight_index(), 2, "末尾で止まる");

        // 行が減っても範囲外を指さない。
        bar.move_highlight(1, 1);
        assert_eq!(bar.highlight_index(), 0);
        bar.move_highlight(1, 0);
        assert_eq!(bar.highlight_index(), 0);
    }

    /// マウスの hover でも同じ選択を動かす。選択の見た目を二つにしない。
    #[test]
    fn the_pointer_moves_the_same_highlight() {
        let mut bar = MenuBar::default();
        bar.toggle_bar();
        bar.highlight(2);
        assert_eq!(bar.highlight_index(), 2);
    }

    /// popup の中身は開いた場所で変わる。
    #[test]
    fn the_popup_entries_follow_where_it_opened() {
        let translator = translator();
        let context = context();

        let item = rows(Open::Popup(Popup::Item), &context, true, &translator, false);
        assert_eq!(
            item.iter().map(|row| row.action).collect::<Vec<_>>(),
            vec![
                ActionId::new(input::ACTIVATE),
                ActionId::new(input::ACTIVATE_NEW_TAB),
                ActionId::new(input::COPY_PATHS),
                ActionId::new(input::COPY_NAMES),
                ActionId::new(input::RENAME),
                ActionId::new(input::REFRESH)
            ]
        );
        for row in item.iter().take(2) {
            assert!(row.enabled, "項目の上ならどちらも使える");
        }

        // 余白では選択解除と読み直し。対象が無いので項目の動作は出さない。
        let background = rows(
            Open::Popup(Popup::Background),
            &context,
            false,
            &translator,
            false,
        );
        assert_eq!(
            background.iter().map(|row| row.action).collect::<Vec<_>>(),
            vec![
                ActionId::new(input::NAVIGATE_BACK),
                ActionId::new(input::NAVIGATE_FORWARD),
                ActionId::new(input::CREATE_FILE),
                ActionId::new(input::CREATE_FOLDER),
                ActionId::new(input::PASTE_LINK),
                ActionId::new(input::PASTE_JUNCTION),
                ActionId::new(input::REFRESH),
                ActionId::new(input::EXIT_SELECTION)
            ]
        );
        let row = |action| {
            background
                .iter()
                .find(|row| row.action == ActionId::new(action))
                .unwrap()
        };
        assert!(
            !row(input::CREATE_FOLDER).enabled,
            "a filesystem path without an accepted identity cannot create a folder"
        );
        assert!(!row(input::CREATE_FILE).enabled);
        assert!(
            !row(input::PASTE_LINK).enabled,
            "filesystem directory でなければ link 作成は使えない"
        );
        assert!(
            !row(input::PASTE_JUNCTION).enabled,
            "filesystem directory でなければ junction 作成は使えない"
        );
        assert!(row(input::REFRESH).enabled, "場所があれば読み直せる");
        assert!(
            !row(input::EXIT_SELECTION).enabled,
            "選択が無ければ解除は使えない"
        );

        let mut accepted_directory = context;
        accepted_directory.create_folder_target = true;
        let accepted_background = rows(
            Open::Popup(Popup::Background),
            &accepted_directory,
            false,
            &translator,
            false,
        );
        assert!(
            accepted_background
                .iter()
                .find(|row| row.action == ActionId::new(input::CREATE_FILE))
                .unwrap()
                .enabled
        );
        assert!(
            accepted_background
                .iter()
                .find(|row| row.action == ActionId::new(input::CREATE_FOLDER))
                .unwrap()
                .enabled
        );
    }

    /// Enter が実行する行は、いま組んだ行と同じであること。
    #[test]
    fn the_highlighted_row_is_the_one_the_menu_shows() {
        let translator = translator();
        let rows = rows(
            Open::Menu(Menu::View),
            &context(),
            false,
            &translator,
            false,
        );
        assert!(rows.len() > 1);

        let mut bar = MenuBar::default();
        bar.toggle_bar();
        bar.show_menu(Menu::View);
        bar.move_highlight(1, rows.len());

        assert_eq!(
            bar.highlighted(&rows).map(|row| row.action),
            Some(rows[1].action)
        );
        assert!(bar.highlighted::<super::Entry>(&[]).is_none());
    }
}
