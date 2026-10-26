//! ウィンドウのタイトルバー。
//!
//! ドラッグ・最小化・最大化・閉じるは platform に任せる。こちらは
//! [`WindowControlArea`] で領域を示すだけで、クリックを自分では処理しない
//! （v5 の titlebar と同じ）。Windows では `WM_NCHITTEST` に応える形で動き、
//! システムのタイトルバーを隠している間のリサイズ枠も platform が計算する。
//!
//! メニューバーはメニューモードのときだけ、ハンバーガーの右へ差し込まれる。誰が menu を
//! 開くかは `shell` が決めるので、ここはハンバーガーの見た目と「押された」ことだけを扱う。
//! タイトルの右の余白も同じで、メニューモードの間は「メニューバーの何もないところ」として
//! 押されたことを伝えるだけ。

use gpui::{
    App, AppContext as _, Context, Div, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Rgba, ScrollAnchor, ScrollHandle, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div,
    prelude::FluentBuilder as _, px,
};
use gpui_filer_resources::IconId;
use gpui_filer_ui::Icon;
use std::rc::Rc;

use crate::bounds_capture::BoundsCapture;
use crate::rename_editor::RenameInput;
use crate::shell::WorkspaceId;
use std::cell::RefCell;

pub(super) struct WorkspaceTab {
    pub(super) id: WorkspaceId,
    pub(super) expected_order: Vec<WorkspaceId>,
    pub(super) bounds: Rc<RefCell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(super) context_bounds: Rc<RefCell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(super) menu_open: bool,
    pub(super) title: String,
    pub(super) active: bool,
    pub(super) transfer_target: bool,
    pub(super) closable: bool,
    pub(super) duplicate_focus: FocusHandle,
    pub(super) duplicate_scroll_anchor: ScrollAnchor,
    pub(super) editor: Option<Entity<RenameInput>>,
}

pub(super) struct WorkspaceRestoreItem {
    pub(super) kind: RestoreItemKind,
    pub(super) history_id: u64,
    pub(super) title: String,
    pub(super) detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestoreItemKind {
    Workspace,
}

#[derive(Clone)]
pub(super) enum WorkspaceIntent {
    WorkspacePointerDown {
        source: WorkspaceId,
        expected_order: Vec<WorkspaceId>,
    },
    Activate(WorkspaceId),
    Add,
    OpenWorkspaceMenu {
        id: WorkspaceId,
        at: gpui::Point<gpui::Pixels>,
    },
    Reorder {
        source: WorkspaceId,
        target: WorkspaceId,
        expected_order: Vec<WorkspaceId>,
    },
    Close(WorkspaceId),
    OpenRestoreMenu,
    RestoreWorkspace(u64),
    BeginRename(WorkspaceId),
    CommitRename(WorkspaceId),
    CancelRename(WorkspaceId),
    CancelWorkspaceTransferOrDrag,
}

#[derive(Clone)]
struct WorkspaceReorderPayload {
    source: WorkspaceId,
    expected_order: Vec<WorkspaceId>,
}

struct WorkspaceReorderGhost {
    title: String,
}

impl Render for WorkspaceReorderGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(12.0))
            .h(px(HEIGHT))
            .flex()
            .items_center()
            .bg(gpui_filer_ui::theme_palette(_cx).surface_raised)
            .text_color(gpui_filer_ui::theme_palette(_cx).text)
            .child(self.title.clone())
    }
}

/// タイトルバーの高さ。
///
/// タッチの目安（44 論理px）より低いので、ウィンドウボタンは**幅**を 44 にして
/// 縦は行いっぱいにする。行そのものを 44 にすると、本文を押し下げすぎる。
pub(super) const HEIGHT: f32 = 44.0;
/// ハンバーガーの幅。
pub(super) const HAMBURGER_WIDTH: f32 = 44.0;
/// ウィンドウボタンの幅。v5 と同じ 44。
const WINDOW_BUTTON_WIDTH: f32 = 44.0;
/// ウィンドウボタンのアイコンの大きさ。v5 と同じ 14。
const WINDOW_ICON_SIZE: f32 = 14.0;

/// タイトルバーの行。
///
/// `menu` はメニューバーで、メニューモードのときだけ差し込む。場所はハンバーガーの右で、
/// 横タブを置く予定だった所と同じ。溢れる分はバーの中で横スクロールする。
///
/// **メニューモード中は、タイトルの右の余白をドラッグ領域にしない。** ここはメニューバーの
/// 何もないところになり、押すと menu モードが終わる（v5 の menu mode と同じ）。ドラッグは
/// モードが終わってからになるが、ウィンドウボタンはそのまま効く。
pub(super) fn bar(
    workspaces: Vec<WorkspaceTab>,
    workspace_scroll: &ScrollHandle,
    restore_empty_label: String,
    restore_menu_open: bool,
    restore_items: Vec<WorkspaceRestoreItem>,
    restore_workspaces_label: String,
    menu: Option<gpui::AnyElement>,
    menu_mode: bool,
    maximized: bool,
    palette: gpui_filer_ui::ThemePalette,
    on_menu: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_workspace: impl Fn(WorkspaceIntent, &mut Window, &mut App) + 'static,
    on_search: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let on_workspace = Rc::new(on_workspace);
    let workspace_strip =
        workspace_strip(workspaces, workspace_scroll, on_workspace.clone(), palette);
    let workspace_add = workspace_add(on_workspace.clone(), palette);
    let workspace_restore = workspace_restore(
        restore_empty_label,
        restore_menu_open,
        restore_items,
        restore_workspaces_label,
        on_workspace.clone(),
        palette,
    );
    let rest = div().flex_1().min_w(px(44.0)).h_full();
    let rest = if menu_mode {
        rest.on_mouse_down(gpui::MouseButton::Left, on_dismiss)
    } else {
        // 通常はドラッグ領域。ダブルクリックの最大化も platform が行う。
        rest.window_control_area(WindowControlArea::Drag)
    };
    div()
        .h(px(HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .bg(palette.surface_base)
        .child(hamburger(palette, on_menu))
        .when(menu_mode, |bar| bar.children(menu))
        .when(!menu_mode, |bar| bar.child(workspace_strip))
        .child(rest)
        .when(!menu_mode, |bar| {
            bar.child(workspace_add).child(workspace_restore)
        })
        .child(search_button(palette, on_search))
        .child(window_button(
            "window-min",
            WindowControlArea::Min,
            IconId::WindowMinimize,
            palette.chrome_hover,
            palette,
        ))
        .child(window_button(
            "window-max",
            WindowControlArea::Max,
            // v5 と同じく、最大化中は戻す印にする。
            if maximized {
                IconId::WindowRestore
            } else {
                IconId::WindowMaximize
            },
            palette.chrome_hover,
            palette,
        ))
        // 閉じるだけホバーを赤にする。他のボタンと同じ濃さだと、危険な操作に見えない。
        .child(window_button(
            "window-close",
            WindowControlArea::Close,
            IconId::WindowClose,
            palette.close_hover,
            palette,
        ))
}

fn workspace_strip(
    workspaces: Vec<WorkspaceTab>,
    workspace_scroll: &ScrollHandle,
    on_workspace: Rc<dyn Fn(WorkspaceIntent, &mut Window, &mut App) + 'static>,
    palette: gpui_filer_ui::ThemePalette,
) -> Stateful<Div> {
    let tabs = workspaces.into_iter().map(|tab| {
        let on_workspace = on_workspace.clone();
        let editing = tab.editor.is_some();
        let menu_open = tab.menu_open;
        let editor_for_keys = tab.editor.clone();
        let id = tab.id;
        let bounds = tab.bounds.clone();
        let expected_order_for_press = tab.expected_order.clone();
        let drag_ghost_title = tab.title.clone();
        let mut button = div()
            .id(("workspace-tab", id.value()))
            .h(px(HEIGHT))
            .px(px(12.0))
            .min_w(px(44.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .text_color(palette.text)
            .text_size(px(12.0))
            .hover(|button| button.bg(palette.chrome_hover))
            .cursor(gpui::CursorStyle::PointingHand)
            .when_some(tab.editor.clone(), |button, input| {
                button.child(div().flex_1().min_w_0().child(input))
            })
            .when(tab.editor.is_none(), |button| button.child(tab.title));
        button = button.bg(if tab.active {
            palette.surface_raised
        } else {
            palette.surface_base
        });
        if tab.transfer_target {
            button = button.border_b_2().border_color(palette.accent);
        }
        let on_double_click = on_workspace.clone();
        let mut button = button.on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &gpui::MouseDownEvent, window, cx| {
                if !editing && event.source == gpui::MouseInputSource::Mouse {
                    on_double_click(
                        WorkspaceIntent::WorkspacePointerDown {
                            source: id,
                            expected_order: expected_order_for_press.clone(),
                        },
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }
                if !editing
                    && event.source == gpui::MouseInputSource::Mouse
                    && event.click_count == 2
                {
                    on_double_click(WorkspaceIntent::BeginRename(id), window, cx);
                    cx.stop_propagation();
                }
            },
        );
        let on_workspace = on_workspace.clone();
        let on_workspace_for_keys = on_workspace.clone();
        button = button.on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
            if editing && !menu_open && !event.keystroke.modifiers.modified() {
                match event.keystroke.key.as_str() {
                    "enter" => {
                        if !editor_for_keys
                            .as_ref()
                            .is_some_and(|input| input.read(cx).is_composing())
                        {
                            on_workspace_for_keys(WorkspaceIntent::CommitRename(id), window, cx);
                            cx.stop_propagation();
                        }
                    }
                    "escape" => {
                        on_workspace_for_keys(
                            WorkspaceIntent::CancelWorkspaceTransferOrDrag,
                            window,
                            cx,
                        );
                        if !editor_for_keys
                            .as_ref()
                            .is_some_and(|input| input.read(cx).is_composing())
                        {
                            on_workspace_for_keys(WorkspaceIntent::CancelRename(id), window, cx);
                            cx.stop_propagation();
                        }
                    }
                    _ => {}
                }
            }
        });
        if !editing {
            let payload = WorkspaceReorderPayload {
                source: id,
                expected_order: tab.expected_order.clone(),
            };
            let ghost_title = drag_ghost_title.clone();
            let expected_order = tab.expected_order.clone();
            button = button
                .on_drag_with_threshold_when(
                    payload,
                    px(8.0),
                    |down| {
                        down.source == gpui::MouseInputSource::Mouse
                            && down.button == gpui::MouseButton::Left
                    },
                    move |_, _, _, cx| {
                        let title = ghost_title.clone();
                        cx.new(|_| WorkspaceReorderGhost { title })
                    },
                )
                .can_drop(move |value, _, _| {
                    value
                        .downcast_ref::<WorkspaceReorderPayload>()
                        .is_some_and(|drag| {
                            drag.source != id && drag.expected_order == expected_order
                        })
                });
            let on_reorder = on_workspace.clone();
            button = button.on_drop::<WorkspaceReorderPayload>(move |drag, window, cx| {
                on_reorder(
                    WorkspaceIntent::Reorder {
                        source: drag.source,
                        target: id,
                        expected_order: drag.expected_order.clone(),
                    },
                    window,
                    cx,
                );
                cx.stop_propagation();
            });
        }
        let on_close = on_workspace.clone();
        let on_face_context = on_workspace.clone();
        let on_face_context_keys = on_face_context.clone();
        let on_activate = on_workspace.clone();
        let on_activate_key = on_workspace.clone();
        let focus = tab.duplicate_focus.clone();
        let scroll_anchor = tab.duplicate_scroll_anchor;
        let menu_anchor =
            tab.bounds
                .borrow()
                .as_ref()
                .map_or(gpui::point(px(0.0), px(0.0)), |bounds| {
                    gpui::point(
                        bounds.origin.x + bounds.size.width / 2.0,
                        bounds.origin.y + bounds.size.height / 2.0,
                    )
                });
        let face = button
            .track_focus(&focus)
            .anchor_scroll(Some(scroll_anchor))
            .focusable()
            .tab_stop(true)
            .focus_visible(|style| style.border_1().border_color(palette.accent))
            .on_click(move |event, window, cx| {
                if !editing && event.standard_click() && event.click_count() == 1 {
                    on_activate_key(WorkspaceIntent::Activate(id), window, cx);
                    cx.stop_propagation();
                }
            })
            .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                let modifiers = event.keystroke.modifiers;
                if editing || menu_open {
                    return;
                }
                if (modifiers.shift && key == "f10")
                    || (!modifiers.modified() && matches!(key, "contextmenu" | "apps" | "menu"))
                {
                    on_face_context_keys(
                        WorkspaceIntent::OpenWorkspaceMenu {
                            id,
                            at: menu_anchor,
                        },
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                } else if !modifiers.modified() && matches!(key, "enter" | "space" | " ") {
                    on_activate(WorkspaceIntent::Activate(id), window, cx);
                    cx.stop_propagation();
                }
            })
            .on_mouse_down(
                gpui::MouseButton::Right,
                move |event: &gpui::MouseDownEvent, window, cx| {
                    on_face_context(
                        WorkspaceIntent::OpenWorkspaceMenu {
                            id,
                            at: event.position,
                        },
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                },
            );
        let face = div().child(BoundsCapture {
            child: face.into_any_element(),
            bounds,
        });
        let context_bounds = tab.context_bounds.clone();
        let on_row_context = on_workspace.clone();
        let row = if tab.closable {
            div()
                .flex()
                .items_center()
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        on_row_context(
                            WorkspaceIntent::OpenWorkspaceMenu {
                                id,
                                at: event.position,
                            },
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    },
                )
                .child(face)
                .child(
                    div()
                        .id(("workspace-close", id.value()))
                        .w(px(44.0))
                        .h(px(HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|button| button.bg(palette.chrome_hover))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .child(
                            Icon::new(IconId::Close)
                                .size(px(12.0))
                                .color(palette.text.into()),
                        )
                        .on_click(move |event, window, cx| {
                            if event.standard_click() {
                                on_close(WorkspaceIntent::Close(id), window, cx);
                                cx.stop_propagation();
                            }
                        }),
                )
        } else {
            div().flex().items_center().child(face)
        };
        div().child(BoundsCapture {
            child: row.into_any_element(),
            bounds: context_bounds,
        })
    });
    div()
        .id("workspace-tab-scroll")
        .flex_shrink(1.0)
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .overflow_x_scroll()
        .track_scroll(workspace_scroll)
        .children(tabs)
}

fn workspace_add(
    on_workspace: Rc<dyn Fn(WorkspaceIntent, &mut Window, &mut App) + 'static>,
    palette: gpui_filer_ui::ThemePalette,
) -> Stateful<Div> {
    div()
        .id("workspace-add")
        .w(px(44.0))
        .h(px(HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .hover(|button| button.bg(palette.chrome_hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(
            Icon::new(IconId::Add)
                .size(px(14.0))
                .color(palette.text.into()),
        )
        .on_click(move |event, window, cx| {
            if event.standard_click() {
                on_workspace(WorkspaceIntent::Add, window, cx);
                cx.stop_propagation();
            }
        })
}

fn workspace_restore(
    restore_empty_label: String,
    restore_menu_open: bool,
    restore_items: Vec<WorkspaceRestoreItem>,
    restore_workspaces_label: String,
    on_workspace: Rc<dyn Fn(WorkspaceIntent, &mut Window, &mut App) + 'static>,
    palette: gpui_filer_ui::ThemePalette,
) -> Div {
    let on_restore = on_workspace.clone();
    div()
        .relative()
        .flex_shrink_0()
        .bg(palette.surface_base)
        .child(
            div()
                .id("workspace-restore")
                .w(px(44.0))
                .h(px(HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                .hover(|button| button.bg(palette.chrome_hover))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(
                    Icon::new(IconId::Delete)
                        .size(px(16.0))
                        .color(palette.text.into()),
                )
                .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |event, window, cx| {
                    if event.standard_click() {
                        on_restore(WorkspaceIntent::OpenRestoreMenu, window, cx);
                        cx.stop_propagation();
                    }
                }),
        )
        .when(restore_menu_open, |menu| {
            let restore_rows = |kind, label: String| {
                let rows = restore_items
                    .iter()
                    .filter(|item| item.kind == kind)
                    .map(|item| {
                        let on_restore = on_workspace.clone();
                        let id = item.history_id;
                        let id_prefix = "restore-workspace";
                        let icon = IconId::Folder;
                        let intent = WorkspaceIntent::RestoreWorkspace(id);
                        div()
                            .id((id_prefix, id))
                            .w(px(240.0))
                            .h(px(44.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_color(palette.text)
                            .hover(|row| row.bg(palette.chrome_hover))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .child(Icon::new(icon).size(px(16.0)).color(palette.text.into()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .justify_center()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(12.0))
                                            .child(item.title.clone()),
                                    )
                                    .when_some(item.detail.clone(), |label, detail| {
                                        label.child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(10.0))
                                                .text_color(palette.text_muted)
                                                .child(detail),
                                        )
                                    }),
                            )
                            .on_click(move |event, window, cx| {
                                if event.standard_click() {
                                    on_restore(intent.clone(), window, cx);
                                    cx.stop_propagation();
                                }
                            })
                    })
                    .collect::<Vec<_>>();
                if rows.is_empty() {
                    None
                } else {
                    Some(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .w(px(240.0))
                                    .px(px(10.0))
                                    .pt(px(7.0))
                                    .pb(px(3.0))
                                    .text_size(px(11.0))
                                    .text_color(palette.text_muted)
                                    .child(label),
                            )
                            .children(rows),
                    )
                }
            };
            let sections = restore_rows(RestoreItemKind::Workspace, restore_workspaces_label);
            let sections = if restore_items.is_empty() {
                vec![
                    div()
                        .w(px(240.0))
                        .h(px(44.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .text_color(palette.text_muted)
                        .child(restore_empty_label)
                        .into_any_element(),
                ]
            } else {
                sections
                    .into_iter()
                    .map(|section| section.into_any_element())
                    .collect()
            };
            menu.child(
                div()
                    .id("workspace-restore-menu")
                    .absolute()
                    .top(px(HEIGHT))
                    .right(px(0.0))
                    .max_h(px(352.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .bg(palette.surface_bar)
                    .border_1()
                    .border_color(palette.border)
                    .children(sections)
                    .on_mouse_down(gpui::MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation()
                    }),
            )
        })
}

/// Shell-wide search はウィンドウ操作ではない。ドラッグ領域へ混ぜず、通常の
/// interactive element として click を Shell へ返す。
fn search_button(
    palette: gpui_filer_ui::ThemePalette,
    on_search: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id("everything-search")
        .w(px(WINDOW_BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .hover(|button| button.bg(palette.chrome_hover))
        .child(
            Icon::new(IconId::Search)
                .size(px(WINDOW_ICON_SIZE))
                .color(palette.text.into()),
        )
        .on_click(on_search)
}

/// ハンバーガー。三本線は div で描く（フォントの glyph に頼らない）。
fn hamburger(
    palette: gpui_filer_ui::ThemePalette,
    on_menu: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let mut bars = div().flex().flex_col().gap(px(3.0)).justify_center();
    for _ in 0..3 {
        bars = bars.child(
            div()
                .w(px(14.0))
                .h(px(1.5))
                .rounded(px(1.0))
                .bg(palette.text),
        );
    }
    div()
        .id("menu-hamburger")
        .w(px(HAMBURGER_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .child(bars)
        .on_click(on_menu)
}

/// ウィンドウボタン 1 つ。
///
/// 動作は `area` が決める。ここは hover とアイコンだけを持つ。アイコンは v5 と同じ
/// 単色 SVG（`currentColor`）で、chrome の文字色で塗る。`hover` を引数にして、
/// 閉じるだけ赤くできるようにしている。
///
/// **カーソルは変えない。** Windows のタイトルバーは矢印のままなので、ここだけ手の形に
/// すると二重に「押せる」と主張して落ち着かない。
fn window_button(
    id: &'static str,
    area: WindowControlArea,
    icon: IconId,
    hover: Rgba,
    palette: gpui_filer_ui::ThemePalette,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(WINDOW_BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .hover(move |button| button.bg(hover))
        .child(
            Icon::new(icon)
                .size(px(WINDOW_ICON_SIZE))
                .color(palette.text.into()),
        )
        .window_control_area(area)
}
