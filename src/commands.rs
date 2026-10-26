//! アプリのコマンド表。action ID を typed intent と availability へ結びつける。
//!
//! key、マウス、toolbar、これから入る menu は同じ入口を通る。ここが知るのは
//! 「今どうなっているか」を写した [`CommandContext`] だけで、view や session そのものは
//! 持たない。filesystem も operation state も持たない。
//!
//! action ID の名前は `input` が持つ（binding の表と同じ語彙）。UI のラベルはここに
//! 置かず、`shell::i18n` が持つ。action ID を label にしない。

use super::ViewLayout;
use super::input;
use gpui_filer_input::ActionId;

/// binding が指すアプリの動作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppIntent {
    ExitSelection,
    ToggleSelectionMode,
    SelectAllEntries,
    NextTab,
    PrevTab,
    ToggleMenu,
    OpenEverythingSearch,
    PickItem {
        toggle: bool,
        range: bool,
    },
    ActivateItem {
        new_tab: bool,
    },
    OpenProperties,
    /// 項目 popup 専用。captured selection は menu route 側で text に解決する。
    CopyPaths,
    /// 項目 popup 専用。captured target の表示名を plain text でコピーする。
    CopyNames,
    /// 矢印キーでのフォーカス移動。`extend` は Shift 付き。
    MoveFocus {
        delta: FocusDelta,
        extend: bool,
    },
    /// 現在の場所を読み直す。navigation とは別 intent。
    Refresh,
    /// 現在の場所の親へ移動する。ドライブ根元では PC へ移る。
    NavigateUp,
    /// ポインタの位置に menu を出す。
    ///
    /// 対象（項目の上か余白か）は呼び出し側が決める。出すこと自体はいつでもできる。
    /// どの行を出すかも、捕まえた対象から決まる。
    ShowContextMenu,
    /// 一覧の表示形式を変える。
    ///
    /// 表示形式の名前は view が持つ [`ViewLayout`] をそのまま使う。同じものを指す
    /// 別の enum をここに作らない。
    SetLayout {
        layout: ViewLayout,
    },
    ToggleColumnVisibility {
        column: crate::sorting::Column,
    },
    MoveColumn {
        column: crate::sorting::Column,
        direction: isize,
    },
    /// 履歴を一つ戻る。
    Back,
    /// 履歴を一つ進む。
    Forward,
    Copy,
    Cut,
    Paste,
    PasteHere,
    PasteLink,
    PasteJunction,
    CreateFolder,
    CreateFile,
    DeleteToTrash,
    UndoFileOperation,
    Rename,
    BatchRename,
    /// Shell-owned application preference. Directory view never mutates it.
    ToggleRenameExtensionSelection,
    ReloadKeymap,
    QuickPreview,
    QuickSearch,
}

/// 矢印キーが動かす向き。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FocusDelta {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// コマンド 1 件。
pub(super) struct Command {
    pub(super) intent: AppIntent,
    /// 実行に要るもの。
    pub(super) needs: Needs,
}

/// 実行に要るもの。コマンド側の語彙で書く。
///
/// 条件を各コマンドの実装に散らすと、menu の disabled と key の消費がずれる。
/// 「今どうなっているか」は [`CommandContext`] が持ち、ここはその読み方だけを決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Needs {
    /// いつでも。今と同じ状態を選ぶことはある。
    Always,
    /// 一覧に項目がある。
    Items,
    /// 対象になる項目がある。クリックした項目、または解決した経路の項目。
    Item,
    /// 選択がある。
    Selection,
    /// 選択がある、または明示的な選択モードが active。
    SelectionOrMode,
    /// 戻れる履歴がある。
    Back,
    /// 進める履歴がある。
    Forward,
    /// 読む場所がある。
    Location,
    /// 親へ移れる場所にいる。
    Up,
    /// タブが複数ある。
    OtherTabs,
    /// 選択中の実 filesystem entry がある。
    OperationSources,
    /// ゴミ箱へ送れる file/folder source がある。
    TrashSources,
    /// Rename は folder を含む、ちょうど一つの実 filesystem entry を受ける。
    RenameTarget,
    /// paste 先になる実 filesystem directory がある。
    Directory,
    /// New Folder requires an accepted identity for its filesystem directory.
    CreateFolderTarget,
}

/// Shell-only directory-tab actions. These never enter the file-list command path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabCommand {
    New,
    Close,
    Select(usize),
}

pub(super) fn tab_command(action: ActionId) -> Option<TabCommand> {
    Some(match action.as_str() {
        input::NEW_TAB => TabCommand::New,
        input::CLOSE_TAB => TabCommand::Close,
        input::SELECT_TAB_1 => TabCommand::Select(0),
        input::SELECT_TAB_2 => TabCommand::Select(1),
        input::SELECT_TAB_3 => TabCommand::Select(2),
        input::SELECT_TAB_4 => TabCommand::Select(3),
        input::SELECT_TAB_5 => TabCommand::Select(4),
        input::SELECT_TAB_6 => TabCommand::Select(5),
        input::SELECT_TAB_7 => TabCommand::Select(6),
        input::SELECT_TAB_8 => TabCommand::Select(7),
        input::SELECT_TAB_9 => TabCommand::Select(8),
        _ => return None,
    })
}

/// availability を判定する材料。
///
/// view や session そのものは渡さない。menu を開いた時に捕捉する対象を増やしすぎない
/// ため、ここに写した事実だけを条件に使う。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CommandContext {
    pub(super) items: usize,
    pub(super) selection: usize,
    pub(super) selection_mode: bool,
    pub(super) can_back: bool,
    pub(super) can_forward: bool,
    pub(super) location: bool,
    /// `Alt+Up` の行き先がある。path の有無だけでは UNC share root と PC を区別できない。
    pub(super) can_up: bool,
    /// タブ数。view はシェルのタブ数を持たないので、view の context では 1 にする
    /// （タブのコマンドは当たらず、シェル側の表で判定される）。
    pub(super) tabs: usize,
    /// Copy/Cut が受け付ける filesystem source の数。folder と link object を含む。
    pub(super) operation_sources: usize,
    /// Trash に渡せる完全な captured selection があるか。link candidate は worker が検証する。
    pub(super) trash_sources: usize,
    pub(super) rename_targets: usize,
    pub(super) directory: bool,
    pub(super) create_folder_target: bool,
}

impl Needs {
    /// 今の状態で満たされているか。
    ///
    /// `has_item` は「対象になる項目があるか」。クリックなら押した項目、キーなら解決した
    /// 経路の項目、popup なら開いた時に捕まえた項目。index そのものは使わない。
    fn satisfied_by(self, context: &CommandContext, has_item: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Items => context.items > 0,
            Self::Item => has_item,
            Self::Selection => context.selection > 0,
            Self::SelectionOrMode => context.selection > 0 || context.selection_mode,
            Self::Back => context.can_back,
            Self::Forward => context.can_forward,
            Self::Location => context.location,
            Self::Up => context.can_up,
            Self::OtherTabs => context.tabs > 1,
            Self::OperationSources => context.operation_sources > 0,
            Self::TrashSources => context.trash_sources > 0,
            Self::RenameTarget => context.rename_targets == 1,
            Self::Directory => context.directory,
            Self::CreateFolderTarget => context.create_folder_target,
        }
    }
}

/// action の定義。知らない action は `None`。
pub(super) fn command(action: ActionId) -> Option<Command> {
    let (intent, needs) = match action.as_str() {
        input::EXIT_SELECTION => (AppIntent::ExitSelection, Needs::Selection),
        input::EXIT_SELECTION_MODE => (AppIntent::ExitSelection, Needs::SelectionOrMode),
        input::TOGGLE_SELECTION_MODE => (AppIntent::ToggleSelectionMode, Needs::Always),
        input::SELECT_ALL_ENTRIES => (AppIntent::SelectAllEntries, Needs::Items),
        input::NEXT_TAB => (AppIntent::NextTab, Needs::OtherTabs),
        input::PREV_TAB => (AppIntent::PrevTab, Needs::OtherTabs),
        input::TOGGLE_MENU => (AppIntent::ToggleMenu, Needs::Always),
        input::OPEN_EVERYTHING_SEARCH => (AppIntent::OpenEverythingSearch, Needs::Always),
        input::PICK => (
            AppIntent::PickItem {
                toggle: false,
                range: false,
            },
            Needs::Item,
        ),
        input::PICK_TOGGLE => (
            AppIntent::PickItem {
                toggle: true,
                range: false,
            },
            Needs::Item,
        ),
        input::PICK_RANGE => (
            AppIntent::PickItem {
                toggle: false,
                range: true,
            },
            Needs::Item,
        ),
        input::ACTIVATE => (AppIntent::ActivateItem { new_tab: false }, Needs::Item),
        input::ACTIVATE_NEW_TAB => (AppIntent::ActivateItem { new_tab: true }, Needs::Item),
        input::OPEN_PROPERTIES => (AppIntent::OpenProperties, Needs::Always),
        input::COPY_PATHS => (AppIntent::CopyPaths, Needs::Item),
        input::COPY_NAMES => (AppIntent::CopyNames, Needs::Item),
        input::FOCUS_UP => (move_focus(FocusDelta::Up, false), Needs::Items),
        input::FOCUS_DOWN => (move_focus(FocusDelta::Down, false), Needs::Items),
        input::FOCUS_LEFT => (move_focus(FocusDelta::Left, false), Needs::Items),
        input::FOCUS_RIGHT => (move_focus(FocusDelta::Right, false), Needs::Items),
        input::FOCUS_UP_EXTEND => (move_focus(FocusDelta::Up, true), Needs::Items),
        input::FOCUS_DOWN_EXTEND => (move_focus(FocusDelta::Down, true), Needs::Items),
        input::FOCUS_LEFT_EXTEND => (move_focus(FocusDelta::Left, true), Needs::Items),
        input::FOCUS_RIGHT_EXTEND => (move_focus(FocusDelta::Right, true), Needs::Items),
        input::FOCUS_HOME => (move_focus(FocusDelta::Home, false), Needs::Items),
        input::FOCUS_END => (move_focus(FocusDelta::End, false), Needs::Items),
        input::FOCUS_HOME_EXTEND => (move_focus(FocusDelta::Home, true), Needs::Items),
        input::FOCUS_END_EXTEND => (move_focus(FocusDelta::End, true), Needs::Items),
        input::FOCUS_PAGE_UP => (move_focus(FocusDelta::PageUp, false), Needs::Items),
        input::FOCUS_PAGE_DOWN => (move_focus(FocusDelta::PageDown, false), Needs::Items),
        input::REFRESH => (AppIntent::Refresh, Needs::Location),
        input::NAVIGATE_UP => (AppIntent::NavigateUp, Needs::Up),
        input::NAVIGATE_BACK => (AppIntent::Back, Needs::Back),
        input::NAVIGATE_FORWARD => (AppIntent::Forward, Needs::Forward),
        input::LAYOUT_ROWS => (
            AppIntent::SetLayout {
                layout: ViewLayout::Rows,
            },
            Needs::Always,
        ),
        input::LAYOUT_TILES => (
            AppIntent::SetLayout {
                layout: ViewLayout::Tiles,
            },
            Needs::Always,
        ),
        input::LAYOUT_ICONS => (
            AppIntent::SetLayout {
                layout: ViewLayout::Icons,
            },
            Needs::Always,
        ),
        input::TOGGLE_COLUMN_KIND => (
            AppIntent::ToggleColumnVisibility {
                column: crate::sorting::Column::Kind,
            },
            Needs::Always,
        ),
        input::TOGGLE_COLUMN_SIZE => (
            AppIntent::ToggleColumnVisibility {
                column: crate::sorting::Column::Size,
            },
            Needs::Always,
        ),
        input::TOGGLE_COLUMN_MODIFIED => (
            AppIntent::ToggleColumnVisibility {
                column: crate::sorting::Column::Modified,
            },
            Needs::Always,
        ),
        input::MOVE_NAME_LEFT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Name,
                direction: -1,
            },
            Needs::Always,
        ),
        input::MOVE_NAME_RIGHT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Name,
                direction: 1,
            },
            Needs::Always,
        ),
        input::MOVE_KIND_LEFT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Kind,
                direction: -1,
            },
            Needs::Always,
        ),
        input::MOVE_KIND_RIGHT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Kind,
                direction: 1,
            },
            Needs::Always,
        ),
        input::MOVE_SIZE_LEFT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Size,
                direction: -1,
            },
            Needs::Always,
        ),
        input::MOVE_SIZE_RIGHT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Size,
                direction: 1,
            },
            Needs::Always,
        ),
        input::MOVE_MODIFIED_LEFT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Modified,
                direction: -1,
            },
            Needs::Always,
        ),
        input::MOVE_MODIFIED_RIGHT => (
            AppIntent::MoveColumn {
                column: crate::sorting::Column::Modified,
                direction: 1,
            },
            Needs::Always,
        ),
        input::COPY => (AppIntent::Copy, Needs::OperationSources),
        input::CUT => (AppIntent::Cut, Needs::OperationSources),
        input::PASTE => (AppIntent::Paste, Needs::Directory),
        input::PASTE_HERE => (AppIntent::PasteHere, Needs::Item),
        input::PASTE_LINK => (AppIntent::PasteLink, Needs::Directory),
        input::PASTE_JUNCTION => (AppIntent::PasteJunction, Needs::Directory),
        input::CREATE_FOLDER => (AppIntent::CreateFolder, Needs::CreateFolderTarget),
        input::CREATE_FILE => (AppIntent::CreateFile, Needs::CreateFolderTarget),
        input::DELETE_TO_TRASH => (AppIntent::DeleteToTrash, Needs::TrashSources),
        // The Shell owns the undo ledger.  It declines an empty ledger after
        // routing, keeping this table free of Shell-owned mutable state.
        input::UNDO_FILE_OPERATION => (AppIntent::UndoFileOperation, Needs::Always),
        input::RENAME => (AppIntent::Rename, Needs::RenameTarget),
        input::BATCH_RENAME => (AppIntent::BatchRename, Needs::Always),
        input::SELECT_EXTENSION_ON_RENAME => {
            (AppIntent::ToggleRenameExtensionSelection, Needs::Always)
        }
        input::RELOAD_KEYMAP => (AppIntent::ReloadKeymap, Needs::Always),
        // focus がなくても、一覧の先頭項目を Quick Preview できるようにする。
        input::QUICK_PREVIEW => (AppIntent::QuickPreview, Needs::Always),
        input::QUICK_SEARCH => (AppIntent::QuickSearch, Needs::Always),
        input::CONTEXT_MENU => (AppIntent::ShowContextMenu, Needs::Always),
        _ => return None,
    };
    Some(Command { intent, needs })
}

fn move_focus(delta: FocusDelta, extend: bool) -> AppIntent {
    AppIntent::MoveFocus { delta, extend }
}

/// action を今の状態で実行できるか。
///
pub(super) fn available(action: ActionId, context: &CommandContext, has_item: bool) -> bool {
    command(action).is_some_and(|command| command.needs.satisfied_by(context, has_item))
}

/// menu を開いた時の selection identity に依存する command か。
///
/// `Needs::Item` は menu が捕捉した item key で解決するため、ここには含めない。
pub(super) fn uses_selection_identity(action: ActionId) -> bool {
    command(action).is_some_and(|command| {
        matches!(
            command.needs,
            Needs::Selection
                | Needs::SelectionOrMode
                | Needs::OperationSources
                | Needs::TrashSources
                | Needs::RenameTarget
        )
    })
}

/// action を今の状態で実行できるなら intent を返す。
///
/// 使えないときは `None`。呼び出し側は当たったことを消費せず、押せない見た目にする。
/// 対象が要るコマンドは、呼び出し側が決めた対象の有無で判定する。
pub(super) fn resolve(
    action: ActionId,
    context: &CommandContext,
    has_item: bool,
) -> Option<AppIntent> {
    command(action)
        .filter(|command| command.needs.satisfied_by(context, has_item))
        .map(|command| command.intent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> CommandContext {
        CommandContext {
            items: 10,
            selection: 0,
            selection_mode: false,
            can_back: false,
            can_forward: false,
            location: true,
            can_up: false,
            tabs: 1,
            operation_sources: 1,
            trash_sources: 1,
            rename_targets: 1,
            directory: true,
            create_folder_target: true,
        }
    }

    fn action(name: &'static str) -> ActionId {
        ActionId::new(name)
    }

    /// Command router に届く keyboard action は、全部ここに定義があること。
    ///
    /// blank-surface drag action は view の gesture adapter が直接扱う。
    #[test]
    fn every_action_name_has_a_command() {
        for name in input::ACTION_NAMES {
            if input::KEYMAP_TABLE.mouse_drag_actions.contains(name) {
                assert!(
                    command(action(name)).is_none(),
                    "gesture-only action: {name}"
                );
            } else {
                assert!(
                    command(action(name)).is_some() || tab_command(action(name)).is_some(),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn the_table_maps_names_to_intents() {
        assert_eq!(tab_command(action(input::NEW_TAB)), Some(TabCommand::New));
        assert_eq!(
            tab_command(action(input::CLOSE_TAB)),
            Some(TabCommand::Close)
        );
        for (name, ordinal) in [
            (input::SELECT_TAB_1, 0),
            (input::SELECT_TAB_2, 1),
            (input::SELECT_TAB_3, 2),
            (input::SELECT_TAB_4, 3),
            (input::SELECT_TAB_5, 4),
            (input::SELECT_TAB_6, 5),
            (input::SELECT_TAB_7, 6),
            (input::SELECT_TAB_8, 7),
            (input::SELECT_TAB_9, 8),
        ] {
            assert_eq!(tab_command(action(name)), Some(TabCommand::Select(ordinal)),);
        }
        assert_eq!(
            command(action(input::EXIT_SELECTION)).map(|command| command.intent),
            Some(AppIntent::ExitSelection)
        );
        assert_eq!(
            command(action(input::FOCUS_HOME_EXTEND)).map(|command| command.intent),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Home,
                extend: true,
            })
        );
        assert_eq!(
            command(action(input::LAYOUT_TILES)).map(|command| command.intent),
            Some(AppIntent::SetLayout {
                layout: ViewLayout::Tiles,
            })
        );
        assert_eq!(
            command(action(input::RELOAD_KEYMAP)).map(|command| command.intent),
            Some(AppIntent::ReloadKeymap)
        );
        assert!(command(action("nope")).is_none());
    }

    /// 選択が無ければ選択解除は使えない。menu の disabled と key の消費が同じ判定になる。
    #[test]
    fn availability_follows_the_context() {
        let mut context = context();
        assert!(!available(action(input::EXIT_SELECTION), &context, false));
        assert!(available(action(input::FOCUS_DOWN), &context, false));
        assert!(available(action(input::REFRESH), &context, false));
        assert!(!available(action(input::NAVIGATE_UP), &context, false));
        context.can_up = true;
        assert!(available(action(input::NAVIGATE_UP), &context, false));

        // 対象が要るコマンドは、呼び出し側が渡した対象で決まる。
        assert!(!available(action(input::PICK), &context, false));
        assert!(available(action(input::PICK), &context, true));

        context.selection = 2;
        assert!(available(action(input::EXIT_SELECTION), &context, false));

        // 履歴の端では戻れない。
        assert!(!available(action(input::NAVIGATE_BACK), &context, false));
        context.can_back = true;
        assert!(available(action(input::NAVIGATE_BACK), &context, false));

        // 空の一覧では矢印が動かない。
        context.items = 0;
        assert!(!available(action(input::FOCUS_DOWN), &context, false));

        // 場所が無ければ読み直せない。
        context.location = false;
        assert!(!available(action(input::REFRESH), &context, false));

        // タブが 1 枚なら切り替えない。
        assert!(!available(action(input::NEXT_TAB), &context, false));
        context.tabs = 2;
        assert!(available(action(input::NEXT_TAB), &context, false));

        // 表示形式はいつでも選べる。
        assert!(available(action(input::LAYOUT_ICONS), &context, false));

        assert!(available(action(input::COPY), &context, false));
        assert!(available(action(input::CUT), &context, false));
        assert!(available(action(input::PASTE), &context, false));
        assert!(available(action(input::DELETE_TO_TRASH), &context, false));
        context.operation_sources = 0;
        assert!(!available(action(input::COPY), &context, false));
        assert!(!available(action(input::CUT), &context, false));
        // Trash availability follows the complete captured selection; link candidates
        // remain subject to worker-side tag and identity admission.
        context.trash_sources = 0;
        assert!(!available(action(input::DELETE_TO_TRASH), &context, false));
        context.directory = false;
        assert!(!available(action(input::PASTE), &context, false));
    }

    #[test]
    fn resolve_returns_the_intent_only_when_available() {
        let context = context();
        assert_eq!(
            resolve(action(input::FOCUS_END), &context, false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::End,
                extend: false,
            })
        );
        assert_eq!(
            resolve(action(input::EXIT_SELECTION), &context, false),
            None
        );
        assert_eq!(
            resolve(action(input::QUICK_PREVIEW), &context, false),
            Some(AppIntent::QuickPreview)
        );
        assert_eq!(resolve(action("nope"), &context, false), None);
    }

    #[test]
    fn create_folder_requires_a_filesystem_directory_with_accepted_identity() {
        let action = action(input::CREATE_FOLDER);
        let mut context = context();
        assert_eq!(
            resolve(action, &context, false),
            Some(AppIntent::CreateFolder)
        );
        context.create_folder_target = false;
        assert_eq!(resolve(action, &context, false), None);
    }

    #[test]
    fn selection_identity_guards_selection_derived_commands() {
        assert!(uses_selection_identity(action(input::EXIT_SELECTION)));
        assert!(uses_selection_identity(action(input::COPY)));
        assert!(uses_selection_identity(action(input::CUT)));
        assert!(uses_selection_identity(action(input::DELETE_TO_TRASH)));
        assert!(uses_selection_identity(action(input::RENAME)));
        assert!(!uses_selection_identity(action(input::ACTIVATE)));
        assert!(!uses_selection_identity(action(input::REFRESH)));
        assert!(!uses_selection_identity(action(
            input::SELECT_EXTENSION_ON_RENAME
        )));
    }
}
