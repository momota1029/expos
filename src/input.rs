//! GPUI の入力イベントを `gpui-filer-input` の型へ正規化するアダプタ。
//!
//! GPUI に依存するのはここだけにして、input crate は GPUI を知らないまま保つ。
//! ここが持つのは「どの chord / クリックがどの action ID を引くか」と、条件に使う
//! scope の語彙。action ID の意味と実行条件は `commands` が持つ。

use gpui::Keystroke;
use gpui_filer_input::{
    ActionId, Bindings, Condition, KeyBinding, KeyChord, MouseBinding, MouseButton, MouseChord,
    ScopeId,
};
pub use gpui_filer_input::{ForeignFileDropIntent, Modifiers, foreign_file_drop_intent};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::keymap;

/// binding の条件に使うスコープ。値そのものに意味はなく、このファイルが対応を決める。
pub(super) mod scope {
    use gpui_filer_input::ScopeId;

    pub const SHELL: ScopeId = ScopeId::new(1);
    pub const FILE_VIEW: ScopeId = ScopeId::new(2);
    pub const ROWS: ScopeId = ScopeId::new(3);
    pub const TILES: ScopeId = ScopeId::new(4);
    pub const ICONS: ScopeId = ScopeId::new(5);
    pub const SELECTION_MODE: ScopeId = ScopeId::new(6);
    /// hover 中の項目。v5 の `file_item` に対応する。
    pub const FILE_ITEM: ScopeId = ScopeId::new(7);
    /// menu が開いている間の配送先。key を一覧へ落とさないための modal path。
    ///
    /// 外部 keymap の条件には出さない（binding を書く場所ではないため）ので、
    /// `SCOPES` には入れない。
    pub const MENU: ScopeId = ScopeId::new(8);
    /// Shell-wide file conflict dialog。menu と別の modal path にする。
    pub const DIALOG: ScopeId = ScopeId::new(9);
    /// folder-tree cascade の内部 modal path。外部 keymap の条件には公開しない。
    pub const FOLDER_TREE: ScopeId = ScopeId::new(10);
    pub const SECTION_FOCUS: ScopeId = ScopeId::new(11);
}

/// keymap ファイルが名前で参照できる scope。
///
/// 名前の source of truth はここ。keymap 側に第二の一覧を作らない。
const SCOPES: &[(&str, ScopeId)] = &[
    ("shell", scope::SHELL),
    ("file_view", scope::FILE_VIEW),
    ("rows", scope::ROWS),
    ("tiles", scope::TILES),
    ("icons", scope::ICONS),
    ("selection_mode", scope::SELECTION_MODE),
    ("file_item", scope::FILE_ITEM),
    ("section_focus", scope::SECTION_FOCUS),
];

/// keymap ファイルが名前で参照できる action。ここに無い名前は診断になる。
///
/// 既定表と同じ定数を並べる。表に足したのにここへ足し忘れるとテストが落ちる。
/// 意味（intent と実行条件）は `commands` が持つ。
pub(super) const ACTION_NAMES: &[&str] = &[
    EXIT_SELECTION,
    EXIT_SELECTION_MODE,
    TOGGLE_SELECTION_MODE,
    TOGGLE_MENU,
    NEXT_TAB,
    PREV_TAB,
    OPEN_EVERYTHING_SEARCH,
    NEW_TAB,
    CLOSE_TAB,
    SELECT_TAB_1,
    SELECT_TAB_2,
    SELECT_TAB_3,
    SELECT_TAB_4,
    SELECT_TAB_5,
    SELECT_TAB_6,
    SELECT_TAB_7,
    SELECT_TAB_8,
    SELECT_TAB_9,
    PICK,
    PICK_TOGGLE,
    PICK_RANGE,
    RECTANGLE_ADD,
    RECTANGLE_TOGGLE,
    ACTIVATE,
    ACTIVATE_NEW_TAB,
    OPEN_PROPERTIES,
    FOCUS_UP,
    FOCUS_DOWN,
    FOCUS_LEFT,
    FOCUS_RIGHT,
    FOCUS_UP_EXTEND,
    FOCUS_DOWN_EXTEND,
    FOCUS_LEFT_EXTEND,
    FOCUS_RIGHT_EXTEND,
    FOCUS_HOME,
    FOCUS_END,
    FOCUS_HOME_EXTEND,
    FOCUS_END_EXTEND,
    FOCUS_PAGE_UP,
    FOCUS_PAGE_DOWN,
    SELECT_ALL_ENTRIES,
    REFRESH,
    NAVIGATE_UP,
    CONTEXT_MENU,
    NAVIGATE_BACK,
    NAVIGATE_FORWARD,
    LAYOUT_ROWS,
    LAYOUT_TILES,
    LAYOUT_ICONS,
    COPY,
    CUT,
    PASTE,
    PASTE_LINK,
    PASTE_JUNCTION,
    CREATE_FOLDER,
    DELETE_TO_TRASH,
    UNDO_FILE_OPERATION,
    RENAME,
    SELECT_EXTENSION_ON_RENAME,
    RELOAD_KEYMAP,
    QUICK_PREVIEW,
    QUICK_SEARCH,
];

/// 外部 keymap を翻訳するときの語彙。
pub(super) const KEYMAP_TABLE: keymap::Table = keymap::Table {
    scopes: SCOPES,
    actions: ACTION_NAMES,
    mouse_drag_actions: &[RECTANGLE_ADD, RECTANGLE_TOGGLE],
};

/// binding が指すアプリの動作。
///
/// 名前はここが持ち、意味（typed intent と実行条件）は `commands` が持つ。
pub(super) const EXIT_SELECTION: &str = "file_view.exit_selection";
pub(super) const EXIT_SELECTION_MODE: &str = "file_view.exit_selection_mode";
pub(super) const TOGGLE_SELECTION_MODE: &str = "file_view.toggle_selection_mode";
pub(super) const NEXT_TAB: &str = "shell.next_tab";
pub(super) const TOGGLE_MENU: &str = "shell.toggle_menu";
pub(super) const PREV_TAB: &str = "shell.prev_tab";
pub(super) const OPEN_EVERYTHING_SEARCH: &str = "shell.open_everything_search";
pub(super) const NEW_TAB: &str = "shell.new_tab";
pub(super) const CLOSE_TAB: &str = "shell.close_tab";
pub(super) const SELECT_TAB_1: &str = "shell.select_tab_1";
pub(super) const SELECT_TAB_2: &str = "shell.select_tab_2";
pub(super) const SELECT_TAB_3: &str = "shell.select_tab_3";
pub(super) const SELECT_TAB_4: &str = "shell.select_tab_4";
pub(super) const SELECT_TAB_5: &str = "shell.select_tab_5";
pub(super) const SELECT_TAB_6: &str = "shell.select_tab_6";
pub(super) const SELECT_TAB_7: &str = "shell.select_tab_7";
pub(super) const SELECT_TAB_8: &str = "shell.select_tab_8";
pub(super) const SELECT_TAB_9: &str = "shell.select_tab_9";
pub(super) const PICK: &str = "file_view.pick";
pub(super) const PICK_TOGGLE: &str = "file_view.pick_toggle";
pub(super) const PICK_RANGE: &str = "file_view.pick_range";
pub(super) const RECTANGLE_ADD: &str = "file_view.rectangle_add";
pub(super) const RECTANGLE_TOGGLE: &str = "file_view.rectangle_toggle";
pub(super) const ACTIVATE: &str = "file_view.activate";
pub(super) const ACTIVATE_NEW_TAB: &str = "file_view.activate_new_tab";
pub(super) const OPEN_PROPERTIES: &str = "file_view.open_properties";
/// Popup-only action; intentionally omitted from ACTION_NAMES and all bindings.
pub(super) const COPY_PATHS: &str = "file_view.copy_paths";
/// Background-popup action; copies the captured directory location as plain text.
pub(super) const COPY_CURRENT_PATH: &str = "file_view.copy_current_path";
/// Popup-only action; intentionally omitted from ACTION_NAMES and all bindings.
pub(super) const COPY_NAMES: &str = "file_view.copy_names";
/// Popup-only action; intentionally omitted from ACTION_NAMES and all bindings.
pub(super) const PROPERTIES: &str = "file_view.properties";
pub(super) const FOCUS_UP: &str = "file_view.focus_up";
pub(super) const FOCUS_DOWN: &str = "file_view.focus_down";
pub(super) const FOCUS_LEFT: &str = "file_view.focus_left";
pub(super) const FOCUS_RIGHT: &str = "file_view.focus_right";
pub(super) const FOCUS_UP_EXTEND: &str = "file_view.focus_up_extend";
pub(super) const FOCUS_DOWN_EXTEND: &str = "file_view.focus_down_extend";
pub(super) const FOCUS_LEFT_EXTEND: &str = "file_view.focus_left_extend";
pub(super) const FOCUS_RIGHT_EXTEND: &str = "file_view.focus_right_extend";
pub(super) const FOCUS_HOME: &str = "file_view.focus_home";
pub(super) const FOCUS_END: &str = "file_view.focus_end";
pub(super) const FOCUS_HOME_EXTEND: &str = "file_view.focus_home_extend";
pub(super) const FOCUS_END_EXTEND: &str = "file_view.focus_end_extend";
pub(super) const FOCUS_PAGE_UP: &str = "file_view.focus_page_up";
pub(super) const FOCUS_PAGE_DOWN: &str = "file_view.focus_page_down";
pub(super) const SELECT_ALL_ENTRIES: &str = "file_view.select_all_entries";
pub(super) const REFRESH: &str = "file_view.refresh";
pub(super) const NAVIGATE_UP: &str = "file_view.navigate_up";
pub(super) const CONTEXT_MENU: &str = "file_view.context_menu";
pub(super) const NAVIGATE_BACK: &str = "file_view.navigate_back";
pub(super) const NAVIGATE_FORWARD: &str = "file_view.navigate_forward";
pub(super) const LAYOUT_ROWS: &str = "view.layout_rows";
pub(super) const LAYOUT_TILES: &str = "view.layout_tiles";
pub(super) const LAYOUT_ICONS: &str = "view.layout_icons";
pub(super) const TOGGLE_COLUMN_KIND: &str = "view.toggle_column_kind";
pub(super) const TOGGLE_COLUMN_SIZE: &str = "view.toggle_column_size";
pub(super) const TOGGLE_COLUMN_MODIFIED: &str = "view.toggle_column_modified";
pub(super) const MOVE_NAME_LEFT: &str = "view.move_name_left";
pub(super) const MOVE_NAME_RIGHT: &str = "view.move_name_right";
pub(super) const MOVE_KIND_LEFT: &str = "view.move_kind_left";
pub(super) const MOVE_KIND_RIGHT: &str = "view.move_kind_right";
pub(super) const MOVE_SIZE_LEFT: &str = "view.move_size_left";
pub(super) const MOVE_SIZE_RIGHT: &str = "view.move_size_right";
pub(super) const MOVE_MODIFIED_LEFT: &str = "view.move_modified_left";
pub(super) const MOVE_MODIFIED_RIGHT: &str = "view.move_modified_right";
/// Shell/menu-only action; intentionally omitted from keymap action names.
pub(super) const TOGGLE_FILE_INFO_PANE: &str = "view.toggle_file_info_pane";
pub(super) const TOGGLE_SHOW_HIDDEN_ITEMS: &str = "view.toggle_show_hidden_items";
pub(super) const TOGGLE_ITEM_CHECK_BOXES: &str = "view.toggle_item_check_boxes";
pub(super) const COPY: &str = "file_view.copy";
pub(super) const CUT: &str = "file_view.cut";
pub(super) const PASTE: &str = "file_view.paste";
/// Item-popup action; targets the captured folder row.
pub(super) const PASTE_HERE: &str = "file_view.paste_here";
/// Clipboard sourcesをこの folder に symbolic link として作る。既定の chord は
/// 持たせず、v5 と同じく destination の context menu から明示的に選ぶ。
pub(super) const PASTE_LINK: &str = "file_view.paste_link";
/// Clipboard folder を NTFS junction として作る。symbolic link と違い、file source
/// を受けないことは operation worker が再検証する。
pub(super) const PASTE_JUNCTION: &str = "file_view.paste_junction";
pub(super) const CREATE_FOLDER: &str = "file_view.create_folder";
pub(super) const CREATE_FILE: &str = "file_view.create_file";
pub(super) const DELETE_TO_TRASH: &str = "file_view.delete_to_trash";
pub(super) const UNDO_FILE_OPERATION: &str = "file_view.undo_file_operation";
pub(super) const RENAME: &str = "file_view.rename";
pub(super) const BATCH_RENAME: &str = "file_view.batch_rename";
pub(super) const SELECT_EXTENSION_ON_RENAME: &str = "settings.select_extension_on_rename";
pub(super) const RELOAD_KEYMAP: &str = "settings.reload_keymap";
pub(super) const QUICK_PREVIEW: &str = "file_view.quick_preview";
pub(super) const QUICK_SEARCH: &str = "file_view.quick_search";

/// 一覧が受け持つキーとマウス。
///
/// 条件に `FILE_VIEW` や `FILE_ITEM` を含めるので、一覧の外へ同じ操作が漏れない。
#[derive(Clone)]
pub(super) struct BindingSnapshot {
    pub(super) generation: u64,
    pub(super) file_view: Arc<Bindings>,
    pub(super) shell: Arc<Bindings>,
}

pub(super) struct KeymapRuntime {
    active: RwLock<Arc<BindingSnapshot>>,
    latest_request: AtomicU64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommitResult {
    Applied {
        generation: u64,
    },
    Rejected {
        generation: u64,
        diagnostics: Vec<String>,
    },
    Stale,
}

pub(super) enum PreparedKeymap {
    Applied(Arc<BindingSnapshot>),
    Rejected(Vec<String>),
}

impl KeymapRuntime {
    pub(super) fn new() -> Self {
        Self {
            active: RwLock::new(Arc::new(default_snapshot(0, None))),
            latest_request: AtomicU64::new(0),
        }
    }

    pub(super) fn snapshot(&self) -> Arc<BindingSnapshot> {
        self.active.read().expect("keymap snapshot lock").clone()
    }

    pub(super) fn begin_reload(&self) -> u64 {
        self.latest_request.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(super) fn prepare(
        generation: u64,
        loaded: Result<keymap::Loaded, Vec<String>>,
    ) -> PreparedKeymap {
        match loaded {
            Ok(loaded) => {
                PreparedKeymap::Applied(Arc::new(default_snapshot(generation, Some(loaded))))
            }
            Err(diagnostics) => PreparedKeymap::Rejected(diagnostics),
        }
    }

    pub(super) fn commit(&self, generation: u64, prepared: PreparedKeymap) -> CommitResult {
        if self.latest_request.load(Ordering::Acquire) != generation {
            return CommitResult::Stale;
        }
        match prepared {
            PreparedKeymap::Applied(next) => {
                debug_assert_eq!(next.generation, generation);
                *self.active.write().expect("keymap snapshot lock") = next;
                CommitResult::Applied { generation }
            }
            PreparedKeymap::Rejected(diagnostics) => CommitResult::Rejected {
                generation,
                diagnostics,
            },
        }
    }
}

fn default_snapshot(generation: u64, user: Option<keymap::Loaded>) -> BindingSnapshot {
    let (user_keys, user_mouse) = user
        .as_ref()
        .map(|loaded| (loaded.keys.clone(), loaded.mouse.clone()))
        .unwrap_or_default();
    let mut keys = user_keys.clone();
    keys.extend(file_view_defaults());
    let mut mouse = user_mouse;
    mouse.extend(file_view_mouse_defaults());
    let file_view = Arc::new(Bindings::new(keys).with_mouse(mouse));
    // user の binding を先に置く。既存の first-match precedence を保つ。
    let mut shell_keys = user_keys;
    shell_keys.extend(shell_defaults());
    BindingSnapshot {
        generation,
        file_view,
        shell: Arc::new(Bindings::new(shell_keys)),
    }
}

fn shell_defaults() -> Vec<KeyBinding> {
    let mut keys = vec![
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "space"),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Scope(scope::SECTION_FOCUS),
            ),
            action: ActionId::new(PICK_TOGGLE),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "f10"),
            when: Condition::Always,
            action: ActionId::new(TOGGLE_MENU),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "tab",
            ),
            when: Condition::Always,
            action: ActionId::new(NEXT_TAB),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                "tab",
            ),
            when: Condition::Always,
            action: ActionId::new(PREV_TAB),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                "f",
            ),
            when: Condition::Always,
            action: ActionId::new(OPEN_EVERYTHING_SEARCH),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "t",
            ),
            when: Condition::Always,
            action: ActionId::new(NEW_TAB),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "w",
            ),
            when: Condition::Always,
            action: ActionId::new(CLOSE_TAB),
        },
    ];
    // Number shortcuts use displayed order (locked tabs are already first).
    for (key, action) in [
        ("1", SELECT_TAB_1),
        ("2", SELECT_TAB_2),
        ("3", SELECT_TAB_3),
        ("4", SELECT_TAB_4),
        ("5", SELECT_TAB_5),
        ("6", SELECT_TAB_6),
        ("7", SELECT_TAB_7),
        ("8", SELECT_TAB_8),
        ("9", SELECT_TAB_9),
    ] {
        keys.push(KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                key,
            ),
            when: Condition::Always,
            action: ActionId::new(action),
        });
    }
    keys
}

/// Tests that need the built-in table without an application runtime.
#[cfg(test)]
pub(super) fn file_view_bindings() -> &'static Bindings {
    static BINDINGS: std::sync::OnceLock<Bindings> = std::sync::OnceLock::new();
    BINDINGS
        .get_or_init(|| Bindings::new(file_view_defaults()).with_mouse(file_view_mouse_defaults()))
}

/// 一覧の既定 binding。
fn file_view_defaults() -> Vec<KeyBinding> {
    let mut keys = vec![
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "a",
            ),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Scope(scope::ROWS),
            ),
            action: ActionId::new(SELECT_ALL_ENTRIES),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "a",
            ),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Scope(scope::TILES),
            ),
            action: ActionId::new(SELECT_ALL_ENTRIES),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "/"),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Scope(scope::ROWS),
            ),
            action: ActionId::new(QUICK_SEARCH),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "/"),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Scope(scope::TILES),
            ),
            action: ActionId::new(QUICK_SEARCH),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "enter"),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(ACTIVATE),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "enter",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(ACTIVATE_NEW_TAB),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                "enter",
            ),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Or(
                    Box::new(Condition::Scope(scope::ROWS)),
                    Box::new(Condition::Scope(scope::TILES)),
                ),
            ),
            action: ActionId::new(OPEN_PROPERTIES),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                "n",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(CREATE_FOLDER),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "escape"),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(EXIT_SELECTION_MODE),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "m"),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(TOGGLE_SELECTION_MODE),
        },
        // v5 の keymap と同じく F5 は再読み込み。navigation と別 intent なので
        // 同じ path の no-op 判定を通さない。
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "f5"),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(REFRESH),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "c",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(COPY),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "x",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(CUT),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "v",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(PASTE),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "delete"),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(DELETE_TO_TRASH),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "z",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(UNDO_FILE_OPERATION),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "f2"),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(RENAME),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                "r",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(BATCH_RENAME),
        },
        // Explorer と同じく Alt+Up は親へ移動。素の Up は一覧のフォーカス移動のままにする。
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                "up",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(NAVIGATE_UP),
        },
        // v5 の keymap と同じく ctrl+left / ctrl+right が履歴。
        // navigation と別 intent なので、同じ path の no-op 判定を通さない。
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "left",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(NAVIGATE_BACK),
        },
        KeyBinding {
            chord: KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                "right",
            ),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(NAVIGATE_FORWARD),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "space"),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                Condition::Scope(scope::SELECTION_MODE),
            ),
            action: ActionId::new(PICK_TOGGLE),
        },
        KeyBinding {
            chord: KeyChord::new(Modifiers::default(), "space"),
            when: and(
                Condition::Scope(scope::FILE_VIEW),
                not(Condition::Scope(scope::SELECTION_MODE)),
            ),
            action: ActionId::new(QUICK_PREVIEW),
        },
    ];
    keys.extend(file_view_keys());
    keys
}

/// 矢印キーの割り当て。v5 の keymap.toml にある file_view の移動と同じ向き。
///
/// Shift 付きは範囲のプレビュー、Home/End は端へ寄せる。
fn file_view_keys() -> Vec<KeyBinding> {
    let view = Condition::Scope(scope::FILE_VIEW);
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    let mut keys = Vec::new();
    for (key, plain, extended) in [
        ("up", FOCUS_UP, FOCUS_UP_EXTEND),
        ("down", FOCUS_DOWN, FOCUS_DOWN_EXTEND),
        ("left", FOCUS_LEFT, FOCUS_LEFT_EXTEND),
        ("right", FOCUS_RIGHT, FOCUS_RIGHT_EXTEND),
        ("home", FOCUS_HOME, FOCUS_HOME_EXTEND),
        ("end", FOCUS_END, FOCUS_END_EXTEND),
    ] {
        keys.push(KeyBinding {
            chord: KeyChord::new(Modifiers::default(), key),
            when: view.clone(),
            action: ActionId::new(plain),
        });
        keys.push(KeyBinding {
            chord: KeyChord::new(shift, key),
            when: view.clone(),
            action: ActionId::new(extended),
        });
    }
    for (layout, action) in [
        (scope::ROWS, FOCUS_PAGE_UP),
        (scope::ROWS, FOCUS_PAGE_DOWN),
        (scope::TILES, FOCUS_PAGE_UP),
        (scope::TILES, FOCUS_PAGE_DOWN),
    ] {
        let key = if action == FOCUS_PAGE_UP {
            "pageup"
        } else {
            "pagedown"
        };
        keys.push(KeyBinding {
            chord: KeyChord::new(Modifiers::default(), key),
            when: Condition::And(Box::new(view.clone()), Box::new(Condition::Scope(layout))),
            action: ActionId::new(action),
        });
    }
    keys
}

/// 項目のクリック割り当ての既定。v5 の keymap.toml にある項目の行に対応する。
///
/// 左クリックの意味は選択モードで変わる。v5 と同じく、通常モードは単一選択、
/// 選択モード中はトグルになる。中クリックは新しいタブで開く。
fn file_view_mouse_defaults() -> Vec<MouseBinding> {
    let item = Condition::Scope(scope::FILE_ITEM);
    let selection_mode = Condition::Scope(scope::SELECTION_MODE);
    let blank = Condition::And(
        Box::new(Condition::Scope(scope::FILE_VIEW)),
        Box::new(Condition::Not(Box::new(Condition::Scope(scope::FILE_ITEM)))),
    );
    vec![
        MouseBinding {
            chord: MouseChord::drag(MouseButton::Left, Modifiers::default()),
            when: blank.clone(),
            action: ActionId::new(RECTANGLE_ADD),
        },
        MouseBinding {
            chord: MouseChord::drag(
                MouseButton::Left,
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
            ),
            when: blank.clone(),
            action: ActionId::new(RECTANGLE_ADD),
        },
        MouseBinding {
            chord: MouseChord::drag(
                MouseButton::Left,
                Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            when: blank.clone(),
            action: ActionId::new(RECTANGLE_TOGGLE),
        },
        MouseBinding {
            chord: MouseChord::drag(
                MouseButton::Left,
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
            ),
            when: blank.clone(),
            action: ActionId::new(RECTANGLE_ADD),
        },
        MouseBinding {
            chord: MouseChord::new(MouseButton::Middle, 1, Modifiers::default()),
            when: item.clone(),
            action: ActionId::new(ACTIVATE_NEW_TAB),
        },
        MouseBinding {
            chord: MouseChord::new(
                MouseButton::Left,
                2,
                Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            when: item.clone(),
            action: ActionId::new(ACTIVATE_NEW_TAB),
        },
        MouseBinding {
            chord: MouseChord::new(MouseButton::Left, 2, Modifiers::default()),
            when: item.clone(),
            action: ActionId::new(ACTIVATE),
        },
        // 文脈 menu はポインタの位置に出る。項目の上と余白で中身が変わるので、
        // 条件を分けて両方の経路に同じ action を割り当てる（v5 の `file_item` と同じ分け方）。
        MouseBinding {
            chord: MouseChord::new(MouseButton::Right, 1, Modifiers::default()),
            when: item.clone(),
            action: ActionId::new(CONTEXT_MENU),
        },
        MouseBinding {
            chord: MouseChord::new(MouseButton::Right, 1, Modifiers::default()),
            when: and(Condition::Scope(scope::FILE_VIEW), not(item.clone())),
            action: ActionId::new(CONTEXT_MENU),
        },
        MouseBinding {
            chord: MouseChord::new(
                MouseButton::Left,
                1,
                Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            when: item.clone(),
            action: ActionId::new(PICK_TOGGLE),
        },
        MouseBinding {
            chord: MouseChord::new(
                MouseButton::Left,
                1,
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
            ),
            when: item.clone(),
            action: ActionId::new(PICK_RANGE),
        },
        MouseBinding {
            chord: MouseChord::new(MouseButton::Left, 1, Modifiers::default()),
            when: and(item.clone(), selection_mode.clone()),
            action: ActionId::new(PICK_TOGGLE),
        },
        MouseBinding {
            chord: MouseChord::new(MouseButton::Left, 1, Modifiers::default()),
            when: and(item, not(selection_mode)),
            action: ActionId::new(PICK),
        },
    ]
}

/// `A & B` を条件式として読みやすく組む。
fn and(lhs: Condition, rhs: Condition) -> Condition {
    Condition::And(Box::new(lhs), Box::new(rhs))
}

fn not(condition: Condition) -> Condition {
    Condition::Not(Box::new(condition))
}

/// GPUI の Keystroke を照合できる形へ移す。
pub(super) fn chord(keystroke: &Keystroke) -> KeyChord {
    KeyChord::new(from_gpui(&keystroke.modifiers), &keystroke.key)
}

/// GPUI のクリックを照合できる形へ移す。
///
/// 修飾キーと回数は down 時の値を使う。up を基準にすると、ボタンを離す前に Ctrl や
/// Shift を離した場合に修飾が落ち、修飾付きのクリックが単一選択に化けてしまう。
/// v5 の mousemap も down の値を使っていた。
pub(super) fn click_chord(event: &gpui::ClickEvent) -> Option<MouseChord> {
    match event {
        // 左クリックだけを扱う。他のボタンは `ClickEvent` に来ないので down 側で引く。
        gpui::ClickEvent::Mouse(mouse) => match press_chord(&mouse.down) {
            Some(chord) if chord.button == MouseButton::Left => Some(chord),
            _ => None,
        },
        gpui::ClickEvent::Touch(touch) => {
            if touch.long_press {
                return None;
            }
            Some(MouseChord::new(
                MouseButton::Left,
                touch.tap_count,
                Modifiers::default(),
            ))
        }
        // キーード由来の click は binding では扱わない。
        gpui::ClickEvent::Keyboard(_) => None,
    }
}

/// GPUI のボタンをこの crate のボタンへ移す。未対応のボタンは `None`。
fn from_gpui_button(button: gpui::MouseButton) -> Option<MouseButton> {
    match button {
        gpui::MouseButton::Left => Some(MouseButton::Left),
        gpui::MouseButton::Middle => Some(MouseButton::Middle),
        gpui::MouseButton::Right => Some(MouseButton::Right),
        _ => None,
    }
}

/// GPUI の押下を照合できる形へ移す。
///
/// 中・右クリックは `ClickEvent` に来ないので、down のイベントから作る。ボタンも
/// 修飾キーも連続回数もイベントから取る。固定値で引くと、keymap に `ctrl+middle` の
/// ような割り当てを書いても永遠に届かない。
pub(super) fn press_chord(event: &gpui::MouseDownEvent) -> Option<MouseChord> {
    Some(MouseChord::new(
        from_gpui_button(event.button)?,
        event.click_count,
        from_gpui(&event.modifiers),
    ))
}

/// `function` は v5 の keymap が扱っていないため、ここでも条件に含めない。
fn from_gpui(modifiers: &gpui::Modifiers) -> Modifiers {
    Modifiers {
        control: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
        platform: modifiers.platform,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{self, AppIntent, CommandContext, FocusDelta};

    /// 一覧に項目があり、対象もある前提の状態。
    fn context() -> CommandContext {
        CommandContext {
            items: 10,
            location: true,
            operation_sources: 1,
            trash_sources: 1,
            directory: true,
            create_folder_target: true,
            ..Default::default()
        }
    }

    fn resolve(key: &str, shift: bool) -> Option<AppIntent> {
        let chord = KeyChord::new(
            Modifiers {
                shift,
                ..Default::default()
            },
            key,
        );
        let scopes = [scope::SHELL, scope::FILE_VIEW, scope::ROWS];
        file_view_bindings()
            .resolve(&chord, &scopes)
            .and_then(|binding| commands::resolve(binding.action, &context(), true))
    }

    #[test]
    fn space_toggles_focused_item_in_selection_mode_and_previews_otherwise() {
        let chord = KeyChord::new(Modifiers::default(), "space");
        let normal_scopes: &[&[ScopeId]] = &[
            &[scope::SHELL, scope::FILE_VIEW, scope::ROWS],
            &[scope::SHELL, scope::FILE_VIEW, scope::TILES],
            &[scope::SHELL, scope::FILE_VIEW, scope::ICONS],
            &[
                scope::SHELL,
                scope::FILE_VIEW,
                scope::ROWS,
                scope::FILE_ITEM,
            ],
        ];

        for &scopes in normal_scopes {
            assert_eq!(
                file_view_bindings()
                    .resolve(&chord, &scopes)
                    .map(|binding| binding.action),
                Some(ActionId::new(QUICK_PREVIEW)),
                "scopes: {scopes:?}"
            );
        }

        for layout in [scope::ROWS, scope::TILES, scope::ICONS] {
            for item_is_hovered in [false, true] {
                let mut scopes = vec![scope::SHELL, scope::FILE_VIEW, layout];
                if item_is_hovered {
                    scopes.push(scope::FILE_ITEM);
                }
                scopes.push(scope::SELECTION_MODE);
                assert_eq!(
                    file_view_bindings()
                        .resolve(&chord, &scopes)
                        .map(|binding| binding.action),
                    Some(ActionId::new(PICK_TOGGLE)),
                    "selection-mode scopes: {scopes:?}"
                );
            }
        }
    }

    #[test]
    fn plain_f10_is_the_shell_menu_entry_and_modified_f10_is_not() {
        let defaults = default_snapshot(0, None);
        let scopes = [scope::SHELL];
        let plain = KeyChord::new(Modifiers::default(), "f10");
        assert_eq!(
            defaults
                .shell
                .resolve(&plain, &scopes)
                .map(|binding| binding.action),
            Some(ActionId::new(TOGGLE_MENU))
        );
        assert_eq!(
            commands::resolve(
                ActionId::new(TOGGLE_MENU),
                &CommandContext::default(),
                false
            ),
            Some(AppIntent::ToggleMenu)
        );
        for modifiers in [
            Modifiers {
                shift: true,
                ..Default::default()
            },
            Modifiers {
                control: true,
                ..Default::default()
            },
            Modifiers {
                alt: true,
                ..Default::default()
            },
        ] {
            assert!(
                defaults
                    .shell
                    .resolve(&KeyChord::new(modifiers, "f10"), &scopes)
                    .is_none()
            );
        }
    }

    #[test]
    fn user_shell_binding_precedes_f10_default_and_can_remap_menu_entry() {
        let overridden = default_snapshot(
            1,
            Some(keymap::Loaded {
                keys: vec![KeyBinding {
                    chord: KeyChord::new(Modifiers::default(), "f10"),
                    when: Condition::Always,
                    action: ActionId::new(NEXT_TAB),
                }],
                mouse: Vec::new(),
            }),
        );
        let scopes = [scope::SHELL];
        assert_eq!(
            overridden
                .shell
                .resolve(&KeyChord::new(Modifiers::default(), "f10"), &scopes)
                .map(|binding| binding.action),
            Some(ActionId::new(NEXT_TAB))
        );

        let remapped = default_snapshot(
            2,
            Some(keymap::Loaded {
                keys: vec![KeyBinding {
                    chord: KeyChord::new(
                        Modifiers {
                            control: true,
                            ..Default::default()
                        },
                        "m",
                    ),
                    when: Condition::Always,
                    action: ActionId::new(TOGGLE_MENU),
                }],
                mouse: Vec::new(),
            }),
        );
        assert_eq!(
            remapped
                .shell
                .resolve(
                    &KeyChord::new(
                        Modifiers {
                            control: true,
                            ..Default::default()
                        },
                        "m"
                    ),
                    &scopes,
                )
                .map(|binding| binding.action),
            Some(ActionId::new(TOGGLE_MENU))
        );
    }

    #[test]
    fn shell_tab_shortcuts_are_bound_in_focused_shell_scope() {
        let bindings = Bindings::new(shell_defaults());
        let scopes = [scope::SHELL];
        for (key, action) in [
            ("t", NEW_TAB),
            ("w", CLOSE_TAB),
            ("1", SELECT_TAB_1),
            ("2", SELECT_TAB_2),
            ("3", SELECT_TAB_3),
            ("4", SELECT_TAB_4),
            ("5", SELECT_TAB_5),
            ("6", SELECT_TAB_6),
            ("7", SELECT_TAB_7),
            ("8", SELECT_TAB_8),
            ("9", SELECT_TAB_9),
        ] {
            assert_eq!(
                bindings
                    .resolve(
                        &KeyChord::new(
                            Modifiers {
                                control: true,
                                ..Default::default()
                            },
                            key
                        ),
                        &scopes,
                    )
                    .map(|binding| binding.action),
                Some(ActionId::new(action)),
                "Ctrl+{key} binding",
            );
        }
        assert_eq!(
            bindings
                .resolve(
                    &KeyChord::new(
                        Modifiers {
                            control: true,
                            ..Default::default()
                        },
                        "tab"
                    ),
                    &scopes,
                )
                .map(|binding| binding.action),
            Some(ActionId::new(NEXT_TAB)),
            "Ctrl+Tab remains the existing next-tab command",
        );
    }

    #[test]
    fn arrow_keys_map_to_their_own_direction() {
        assert_eq!(
            resolve("up", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Up,
                extend: false,
            })
        );
        assert_eq!(
            resolve("down", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Down,
                extend: false,
            })
        );
        assert_eq!(
            resolve("left", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Left,
                extend: false,
            })
        );
        assert_eq!(
            resolve("right", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Right,
                extend: false,
            })
        );
    }

    /// 実際に登録されている chord と action の組をそのまま確かめる。
    /// 上のテストは `intent` の対応しか見ていないので、binding 側の取り違えを拾う。
    #[test]
    fn registered_chords_name_the_matching_direction() {
        let expected = [
            ("up", "file_view.focus_up"),
            ("down", "file_view.focus_down"),
            ("left", "file_view.focus_left"),
            ("right", "file_view.focus_right"),
            ("home", "file_view.focus_home"),
            ("end", "file_view.focus_end"),
        ];
        let scopes = [scope::SHELL, scope::FILE_VIEW, scope::ROWS];
        for (key, action) in expected {
            let chord = KeyChord::new(Modifiers::default(), key);
            let resolved = file_view_bindings()
                .resolve(&chord, &scopes)
                .unwrap_or_else(|| panic!("no binding for {key}"));
            assert_eq!(resolved.action.as_str(), action, "key {key}");
        }
    }

    #[test]
    fn shift_arrows_extend_the_same_direction() {
        for (key, delta) in [
            ("up", FocusDelta::Up),
            ("down", FocusDelta::Down),
            ("left", FocusDelta::Left),
            ("right", FocusDelta::Right),
        ] {
            assert_eq!(
                resolve(key, true),
                Some(AppIntent::MoveFocus {
                    delta,
                    extend: true,
                }),
                "shift+{key}"
            );
        }
    }

    #[test]
    fn home_and_end_reach_the_edges() {
        assert_eq!(
            resolve("home", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Home,
                extend: false,
            })
        );
        assert_eq!(
            resolve("end", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::End,
                extend: false,
            })
        );
    }

    fn resolve_item_mouse(
        button: MouseButton,
        count: usize,
        modifiers: Modifiers,
    ) -> Option<AppIntent> {
        let chord = MouseChord::new(button, count, modifiers);
        let scopes = [
            scope::SHELL,
            scope::FILE_VIEW,
            scope::ROWS,
            scope::FILE_ITEM,
        ];
        file_view_bindings()
            .resolve_mouse(&chord, &scopes)
            .and_then(|binding| commands::resolve(binding.action, &context(), true))
    }

    #[test]
    fn middle_click_opens_an_item_in_a_new_tab() {
        // v5 の keymap と同じく、中クリックは新しいタブで開く。
        assert_eq!(
            resolve_item_mouse(MouseButton::Middle, 1, Modifiers::default()),
            Some(AppIntent::ActivateItem { new_tab: true })
        );
    }

    #[test]
    fn middle_click_needs_the_item_scope() {
        // 余白の中クリックには割り当てが無い。v5 も同じだった。
        let chord = MouseChord::new(MouseButton::Middle, 1, Modifiers::default());
        let scopes = [scope::SHELL, scope::FILE_VIEW, scope::ROWS];
        assert!(
            file_view_bindings()
                .resolve_mouse(&chord, &scopes)
                .is_none()
        );
    }

    #[test]
    fn plain_left_click_still_picks() {
        assert_eq!(
            resolve_item_mouse(MouseButton::Left, 1, Modifiers::default()),
            Some(AppIntent::PickItem {
                toggle: false,
                range: false,
            })
        );
    }

    #[test]
    fn f5_refreshes_the_current_location() {
        // v5 の keymap と同じく F5 は再読み込み。
        assert_eq!(resolve("f5", false), Some(AppIntent::Refresh));
        // Shift 付きは別の割り当てが無いので解決されない。
        assert_eq!(resolve("f5", true), None);
    }

    #[test]
    fn alt_up_navigates_to_the_parent_without_replacing_focus_up() {
        assert_eq!(
            resolve("up", false),
            Some(AppIntent::MoveFocus {
                delta: FocusDelta::Up,
                extend: false,
            })
        );
        let chord = KeyChord::new(
            Modifiers {
                alt: true,
                ..Default::default()
            },
            "up",
        );
        let scopes = [scope::SHELL, scope::FILE_VIEW, scope::ROWS];
        let action = file_view_bindings()
            .resolve(&chord, &scopes)
            .unwrap()
            .action;
        assert_eq!(
            commands::resolve(
                action,
                &CommandContext {
                    can_up: true,
                    ..context()
                },
                false,
            ),
            Some(AppIntent::NavigateUp)
        );
    }

    #[test]
    fn control_arrows_move_through_the_history() {
        // v5 の keymap と同じく ctrl+left / ctrl+right。
        // 履歴の端の判定は `commands` のテストが見る。ここは binding の対応だけ。
        let history = |key: &str| {
            let chord = KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                key,
            );
            let scopes = [scope::SHELL, scope::FILE_VIEW, scope::ROWS];
            let context = CommandContext {
                can_back: true,
                can_forward: true,
                ..context()
            };
            file_view_bindings()
                .resolve(&chord, &scopes)
                .and_then(|binding| commands::resolve(binding.action, &context, true))
        };
        assert_eq!(history("left"), Some(AppIntent::Back));
        assert_eq!(history("right"), Some(AppIntent::Forward));
        // 修飾なしの矢印はフォーカス移動のまま。
        assert_ne!(resolve("left", false), Some(AppIntent::Back));
    }

    #[test]
    fn control_clipboard_keys_reach_the_typed_file_intents() {
        let resolve_clipboard = |key: &str| {
            let chord = KeyChord::new(
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                key,
            );
            let scopes = [scope::SHELL, scope::FILE_VIEW, scope::ROWS];
            file_view_bindings()
                .resolve(&chord, &scopes)
                .and_then(|binding| commands::resolve(binding.action, &context(), false))
        };
        assert_eq!(resolve_clipboard("c"), Some(AppIntent::Copy));
        assert_eq!(resolve_clipboard("x"), Some(AppIntent::Cut));
        assert_eq!(resolve_clipboard("v"), Some(AppIntent::Paste));
    }

    #[test]
    fn ctrl_shift_n_reaches_create_folder_only_from_the_file_view_scope() {
        let chord = KeyChord::new(
            Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            "n",
        );
        let binding = file_view_bindings()
            .resolve(&chord, &[scope::SHELL, scope::FILE_VIEW])
            .expect("Ctrl+Shift+N binding");
        assert_eq!(binding.action, ActionId::new(CREATE_FOLDER));
        assert_eq!(
            commands::resolve(binding.action, &context(), false),
            Some(AppIntent::CreateFolder)
        );
        assert!(
            file_view_bindings()
                .resolve(&chord, &[scope::SHELL])
                .is_none()
        );
        assert!(
            file_view_bindings()
                .resolve(&chord, &[scope::SHELL, scope::MENU])
                .is_none()
        );
    }

    #[test]
    fn delete_key_reaches_the_trash_intent() {
        assert_eq!(resolve("delete", false), Some(AppIntent::DeleteToTrash));
    }

    /// keymap の scope 名は重複しないこと。
    #[test]
    fn the_keymap_scope_names_are_unique() {
        for (index, (name, _)) in SCOPES.iter().enumerate() {
            assert!(
                !SCOPES[..index].iter().any(|(earlier, _)| earlier == name),
                "{name}"
            );
            assert!(!name.is_empty(), "scope names are not empty");
        }
    }

    /// user の binding は既定より先に置く。
    ///
    /// `Bindings::resolve` は登録順の最初の一致を採るので、これが上書きの規則になる。
    #[test]
    fn a_user_binding_placed_first_overrides_the_default() {
        let chord = KeyChord::new(Modifiers::default(), "escape");
        let mut keys = vec![KeyBinding {
            chord: chord.clone(),
            when: Condition::Scope(scope::FILE_VIEW),
            action: ActionId::new(EXIT_SELECTION),
        }];
        keys.extend(file_view_defaults());

        let bindings = Bindings::new(keys);
        let scopes = [scope::SHELL, scope::FILE_VIEW];
        assert_eq!(
            bindings
                .resolve(&chord, &scopes)
                .map(|binding| binding.action),
            Some(ActionId::new(EXIT_SELECTION))
        );
    }

    /// マウスも同じ規則で上書きできる。
    #[test]
    fn a_user_mouse_binding_placed_first_overrides_the_default() {
        let chord = MouseChord::new(MouseButton::Left, 1, Modifiers::default());
        let mut mouse = vec![MouseBinding {
            chord: chord.clone(),
            when: Condition::Scope(scope::FILE_ITEM),
            action: ActionId::new(ACTIVATE),
        }];
        mouse.extend(file_view_mouse_defaults());

        let bindings = Bindings::new(Vec::new()).with_mouse(mouse);
        let scopes = [
            scope::SHELL,
            scope::FILE_VIEW,
            scope::ROWS,
            scope::FILE_ITEM,
        ];
        assert_eq!(
            bindings
                .resolve_mouse(&chord, &scopes)
                .map(|binding| binding.action),
            Some(ActionId::new(ACTIVATE))
        );
    }

    /// 押下の chord はイベントのボタン・修飾キー・回数から作る。
    ///
    /// 固定値で引くと、keymap に `ctrl+middle` や `right` を書いても届かない。
    #[test]
    fn a_press_chord_uses_the_event_button_and_count() {
        let control = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        let event = gpui::MouseDownEvent {
            source: gpui::MouseInputSource::Mouse,
            button: gpui::MouseButton::Right,
            position: gpui::point(gpui::px(0.0), gpui::px(0.0)),
            modifiers: control,
            click_count: 2,
            first_mouse: false,
        };

        assert_eq!(
            press_chord(&event),
            Some(MouseChord::new(
                MouseButton::Right,
                2,
                Modifiers {
                    control: true,
                    ..Default::default()
                }
            ))
        );
    }

    fn loaded_keymap(
        file_key: &str,
        file_action: &'static str,
        shell_key: &str,
        shell_action: &'static str,
    ) -> keymap::Loaded {
        keymap::Loaded {
            keys: vec![
                KeyBinding {
                    chord: KeyChord::new(Modifiers::default(), file_key),
                    when: Condition::Scope(scope::FILE_VIEW),
                    action: ActionId::new(file_action),
                },
                KeyBinding {
                    chord: KeyChord::new(Modifiers::default(), shell_key),
                    when: Condition::Always,
                    action: ActionId::new(shell_action),
                },
            ],
            mouse: Vec::new(),
        }
    }

    #[test]
    fn valid_reload_changes_both_domains_together() {
        let runtime = KeymapRuntime::new();
        let generation = runtime.begin_reload();
        assert!(matches!(
            runtime.commit(
                generation,
                KeymapRuntime::prepare(
                    generation,
                    Ok(loaded_keymap("f6", REFRESH, "f7", NEXT_TAB))
                )
            ),
            CommitResult::Applied { .. }
        ));
        let generation = runtime.begin_reload();
        assert!(matches!(
            runtime.commit(
                generation,
                KeymapRuntime::prepare(
                    generation,
                    Ok(loaded_keymap("f6", NAVIGATE_UP, "f7", PREV_TAB))
                )
            ),
            CommitResult::Applied { .. }
        ));
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.generation, generation);
        assert!(
            snapshot
                .file_view
                .resolve(
                    &KeyChord::new(Modifiers::default(), "f6"),
                    &[scope::FILE_VIEW]
                )
                .is_some_and(|binding| binding.action == ActionId::new(NAVIGATE_UP))
        );
        assert!(
            snapshot
                .shell
                .resolve(&KeyChord::new(Modifiers::default(), "f7"), &[scope::SHELL])
                .is_some_and(|binding| binding.action == ActionId::new(PREV_TAB))
        );
    }

    #[test]
    fn invalid_reload_preserves_last_good_snapshot() {
        let runtime = KeymapRuntime::new();
        let generation = runtime.begin_reload();
        runtime.commit(
            generation,
            KeymapRuntime::prepare(generation, Ok(loaded_keymap("f6", REFRESH, "f7", NEXT_TAB))),
        );
        let before = runtime.snapshot();
        let generation = runtime.begin_reload();
        assert!(matches!(
            runtime.commit(
                generation,
                KeymapRuntime::prepare(
                    generation,
                    Err(vec!["keymap.toml: binding 1: bad".into()]),
                ),
            ),
            CommitResult::Rejected { diagnostics, .. } if diagnostics.len() == 1
        ));
        assert!(Arc::ptr_eq(&before, &runtime.snapshot()));
    }

    #[test]
    fn initial_invalid_keeps_defaults_without_partial_application() {
        let runtime = KeymapRuntime::new();
        assert!(matches!(
            runtime.commit(
                0,
                KeymapRuntime::prepare(0, Err(vec!["one".into(), "two".into()])),
            ),
            CommitResult::Rejected { diagnostics, .. } if diagnostics.len() == 2
        ));
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.generation, 0);
        assert!(
            snapshot
                .file_view
                .resolve(
                    &KeyChord::new(Modifiers::default(), "f5"),
                    &[scope::FILE_VIEW]
                )
                .is_some()
        );
    }

    #[test]
    fn stale_reload_is_discarded_and_old_arc_remains_safe() {
        let runtime = KeymapRuntime::new();
        let old = runtime.snapshot();
        let first = runtime.begin_reload();
        let second = runtime.begin_reload();
        assert_eq!(
            runtime.commit(
                first,
                KeymapRuntime::prepare(first, Ok(loaded_keymap("f6", REFRESH, "f7", NEXT_TAB)),),
            ),
            CommitResult::Stale
        );
        assert_eq!(runtime.snapshot().generation, 0);
        assert!(
            old.file_view
                .resolve(
                    &KeyChord::new(Modifiers::default(), "f5"),
                    &[scope::FILE_VIEW]
                )
                .is_some()
        );
        assert!(matches!(
            runtime.commit(
                second,
                KeymapRuntime::prepare(second, Ok(loaded_keymap("f6", REFRESH, "f7", PREV_TAB)),),
            ),
            CommitResult::Applied { .. }
        ));
        assert_eq!(old.generation, 0);
    }
}
