//! menu の見た目。
//!
//! ハンバーガーで出るメニューバー（タイトルバーの中）と、そこから開く dropdown、
//! ポインタの位置に出る popup の div を組み立てる。どの面に何を出すかは `menu` が決める。
//!
//! dropdown を overlay として重ねるのは、タイトルバーが固定高さで、その子として絶対配置
//! した面が本文の下へ潜ってしまうため。v5 の `MainMenu` も同じ理由で overlay に置いていた。

use super::*;
use gpui::StatefulInteractiveElement;
use gpui_filer_resources::IconId;
use gpui_filer_ui::horizontal_scrollbar;

/// dropdown の最小幅。
const PANEL_WIDTH: f32 = 176.0;
const MENU_ROW_PADDING_X: f32 = 12.0;
const MENU_ROW_GAP: f32 = 8.0;
const MENU_ICON_SIZE: f32 = 16.0;
const MENU_FONT_SIZE: f32 = 14.0;
const SHORTCUT_FONT_SIZE: f32 = 12.0;
/// dropdown の 1 行の高さ。
pub(super) const ROW_HEIGHT: f32 = 32.0;
/// Touch-invoked context-menu actions need the same minimum hit height as other touch controls.
pub(super) const TOUCH_CONTEXT_ROW_HEIGHT: f32 = 44.0;
pub(super) const VIEW_ROW_HEIGHT: f32 = 44.0;
pub(super) const REOPEN_ROW_HEIGHT: f32 = 44.0;
const POPUP_PADDING: f32 = 6.0;
const POPUP_BORDER: f32 = 1.0;
const POPUP_INSET: f32 = POPUP_PADDING + POPUP_BORDER;
const POPUP_PANEL_CHROME_HEIGHT: f32 = POPUP_INSET * 2.0;
const MENU_SEPARATOR_HEIGHT: f32 = 9.0;
/// メニューバーのタイトルの左右の余白。
const TITLE_PADDING_X: f32 = 12.0;
/// メニューバーのタイトルの最小幅。タッチの目安（44）を下回らない。
const TITLE_MIN_WIDTH: f32 = 44.0;
const MENU_VIEWPORT_MARGIN: f32 = 8.0;
const POPUP_POINTER_OFFSET: f32 = 12.0;

fn viewport_margin(extent: f32) -> f32 {
    MENU_VIEWPORT_MARGIN.min((extent / 4.0).max(0.0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewMenuGeometry {
    root_left: f32,
    root_column_width: f32,
    child_column_width: f32,
    child_side: ChildSide,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ChildSide {
    Left,
    Right,
}

fn view_menu_geometry(
    anchor: f32,
    viewport_width: f32,
    requested_root_width: f32,
    requested_child_width: f32,
) -> ViewMenuGeometry {
    let margin = viewport_margin(viewport_width);
    let usable_width = (viewport_width - margin * 2.0).max(0.0);
    // 子メニュー幅でルート列を縮めると、行のヒット領域が開閉中に動いてしまう。
    // 半分を子列の安定した予約幅にし、実際の子列だけを予約内に収める。
    let root_column_width = requested_root_width.min(usable_width / 2.0);
    let child_reservation = (usable_width - root_column_width).min(usable_width / 2.0);
    let child_column_width = requested_child_width.min(child_reservation);
    let rightmost_root_left = (viewport_width - margin - root_column_width).max(margin);
    let leftmost_root_left = (margin + child_reservation).min(rightmost_root_left);
    let rightmost_right_root_left =
        (viewport_width - margin - root_column_width - child_reservation).max(margin);
    let child_side = if anchor.clamp(margin, rightmost_root_left) <= rightmost_right_root_left {
        ChildSide::Right
    } else if anchor - child_reservation >= margin {
        ChildSide::Left
    } else {
        ChildSide::Right
    };
    let root_left = match child_side {
        ChildSide::Right => anchor.clamp(margin, rightmost_right_root_left),
        ChildSide::Left => anchor.clamp(leftmost_root_left, rightmost_root_left),
    };
    ViewMenuGeometry {
        root_left,
        root_column_width,
        child_column_width,
        child_side,
    }
}

fn menu_column_width(rows: &[menu::Row], shortcuts: &[Option<String>], window: &Window) -> f32 {
    rows.iter()
        .zip(shortcuts)
        .map(|(row, shortcut)| {
            let trailing_width = if matches!(row, menu::Row::Branch { .. }) {
                MENU_ROW_GAP + MENU_ICON_SIZE
            } else {
                shortcut.as_ref().map_or(0.0, |label| {
                    MENU_ROW_GAP + text_width(label, SHORTCUT_FONT_SIZE, window).ceil()
                })
            };
            text_width(row.label(), MENU_FONT_SIZE, window).ceil()
                + POPUP_PANEL_CHROME_HEIGHT
                + MENU_ROW_PADDING_X * 2.0
                + MENU_ROW_GAP
                + MENU_ICON_SIZE
                + trailing_width
        })
        .fold(PANEL_WIDTH, f32::max)
}

fn popup_border(palette: gpui_filer_ui::ThemePalette) -> gpui::Rgba {
    // テーマの border はアクセント色にもなるため、ポップアップの輪郭は面と文字の色から作る。
    palette.surface_raised.blend(palette.text.opacity(0.16))
}

pub(super) fn menu_row_top(index: usize, row_height: f32, separators: &[usize]) -> f32 {
    index as f32 * row_height
        + separators.partition_point(|&before| before <= index) as f32 * MENU_SEPARATOR_HEIGHT
}

fn popup_panel(palette: gpui_filer_ui::ThemePalette) -> gpui::Div {
    div()
        .absolute()
        .p(px(POPUP_PADDING))
        .flex()
        .flex_col()
        .text_size(px(MENU_FONT_SIZE))
        .rounded(px(8.0))
        .border_1()
        .border_color(popup_border(palette))
        .bg(palette.surface_raised)
        .shadow(vec![
            gpui::BoxShadow::new(px(0.0), px(4.0), gpui::black().opacity(0.24))
                .blur_radius(px(12.0)),
        ])
        .occlude()
}

fn action_icon(action: ActionId) -> Option<IconId> {
    Some(match action.as_str() {
        input::ACTIVATE | input::ACTIVATE_NEW_TAB => IconId::Open,
        input::COPY => IconId::Copy,
        input::COPY_NAMES | input::COPY_PATHS | input::COPY_CURRENT_PATH => IconId::CopyPath,
        input::CUT => IconId::Cut,
        input::PASTE | input::PASTE_HERE => IconId::Paste,
        input::PASTE_LINK | input::PASTE_JUNCTION | menu::CREATE_SHORTCUT => IconId::CreateLink,
        input::RENAME => IconId::Rename,
        input::DELETE_TO_TRASH => IconId::Delete,
        input::PROPERTIES => IconId::Properties,
        input::CREATE_FOLDER => IconId::NewFolder,
        input::CREATE_FILE => IconId::NewFile,
        input::REFRESH => IconId::Refresh,
        input::NAVIGATE_BACK => IconId::Back,
        input::NAVIGATE_FORWARD => IconId::Forward,
        menu::OPEN_IN_NEW_PANE => IconId::SplitPane,
        _ => return None,
    })
}

fn shortcut_label(chord: &gpui_filer_input::KeyChord) -> String {
    let mut label = String::new();
    for (pressed, name) in [
        (chord.modifiers.control, "Ctrl+"),
        (chord.modifiers.alt, "Alt+"),
        (chord.modifiers.shift, "Shift+"),
        (chord.modifiers.platform, "Super+"),
    ] {
        if pressed {
            label.push_str(name);
        }
    }
    let mut key = chord.key.as_str().chars();
    if let Some(first) = key.next() {
        label.push(first.to_ascii_uppercase());
        label.push_str(key.as_str());
    }
    label
}

pub(super) fn reveal_scroll_offset(
    current_top: f32,
    row_top: f32,
    row_height: f32,
    viewport_height: f32,
    content_height: f32,
) -> f32 {
    if content_height <= viewport_height {
        return 0.0;
    }
    let next_top = if row_height >= viewport_height || row_top < current_top {
        row_top
    } else if row_top + row_height > current_top + viewport_height {
        row_top + row_height - viewport_height
    } else {
        current_top
    };
    next_top.clamp(0.0, content_height - viewport_height)
}

fn popup_position(anchor: f32, panel_extent: f32, viewport_extent: f32) -> f32 {
    let margin = viewport_margin(viewport_extent);
    let last = (viewport_extent - margin - panel_extent).max(margin);
    let after = anchor + POPUP_POINTER_OFFSET;
    let before = anchor - POPUP_POINTER_OFFSET - panel_extent;
    if after <= last {
        after.max(margin)
    } else if before >= margin {
        before.min(last)
    } else {
        after.clamp(margin, last)
    }
}

fn context_submenu_geometry(
    root_left: f32,
    viewport_width: f32,
    root_width: f32,
    requested_child_width: f32,
) -> ViewMenuGeometry {
    let margin = viewport_margin(viewport_width);
    let left_space = (root_left - margin).max(0.0);
    let right_space = (viewport_width - margin - root_left - root_width).max(0.0);
    let child_side = if requested_child_width <= right_space || right_space >= left_space {
        ChildSide::Right
    } else {
        ChildSide::Left
    };
    let child_reservation = match child_side {
        ChildSide::Right => right_space,
        ChildSide::Left => left_space,
    };
    ViewMenuGeometry {
        root_left,
        root_column_width: root_width,
        child_column_width: requested_child_width.min(child_reservation),
        child_side,
    }
}

/// メニューバーに並ぶタイトル 1 つ。
///
/// 幅は文字を測って決め、描画にも同じ値を使う。dropdown の左端と、描かれたタイトルの
/// 左端が同じ計算から出るようにしておく。
pub(super) struct MenuTitle {
    pub(super) menu: menu::Menu,
    label: String,
    width: f32,
}

impl FilerShell {
    #[cfg(test)]
    pub(super) fn context_menu_row_point_for_test(
        &mut self,
        cx: &mut Context<Self>,
        matches_row: impl FnMut(&menu::Row) -> bool,
    ) -> gpui::Point<gpui::Pixels> {
        let open = self.menu.open().expect("open context menu");
        let child = self.menu.view_submenu().is_some();
        let (rows, separators) = if child {
            (self.menu_rows(cx), Vec::new())
        } else {
            let root = self.context_menu_root_rows(self.menu_entries(cx));
            (root.rows, root.separators)
        };
        let index = rows
            .iter()
            .position(matches_row)
            .expect("rendered menu row");
        let scroll = self.menu_panel_scroll(open, child);
        let bounds = scroll.bounds();
        let height = self.menu_row_height(open);
        let point = gpui::point(
            bounds.left() + px(24.0).min(bounds.size.width / 2.0),
            bounds.top()
                + scroll.offset().y
                + px(menu_row_top(index, height, &separators) + height / 2.0),
        );
        assert!(
            bounds.contains(&point),
            "menu row {index} is outside {bounds:?}"
        );
        point
    }

    /// メニューバーに並べるタイトル。幅を測るのはここ 1 回だけ。
    pub(super) fn menu_titles(&self, window: &Window, cx: &Context<Self>) -> Vec<MenuTitle> {
        Self::visible_menus(cx)
            .iter()
            .map(|menu| {
                let label = menu.label(&self.translator);
                let width = title_width(&label, window);
                MenuTitle {
                    menu: *menu,
                    label,
                    width,
                }
            })
            .collect()
    }

    /// タイトルバーへ差し込むメニューバー。メニューモードのときだけ出す。
    ///
    /// タイトルの hover でその menu を開き、click でも同じことをする。hover は指を乗せた
    /// 時点で開く（v5 と同じ）ので、開いたあとは指を移すだけで切り替わる。touch には
    /// hover が来ないので click が入口になる。
    ///
    /// 幅が足りないときはバーが縮んで**横スクロール**になる（タイトルは測った幅を保つ）。
    /// メニューは増えうる（File / Help に項目ができたとき）ので、溢れた分を隠さない。
    pub(super) fn render_menu_bar(
        &mut self,
        titles: &[MenuTitle],
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.menu.bar_visible() {
            return None;
        }
        let open = self.menu.open_menu();
        let mut row = div()
            .id("menu-bar")
            .h_full()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&self.menu_scroll);
        for (index, title) in titles.iter().enumerate() {
            let menu = title.menu;
            let active = open == Some(menu);
            row = row.child(
                div()
                    .id(("menu-title", index))
                    .w(px(title.width))
                    // 測った幅を保つ。ここが縮むと、dropdown の左端の計算と食い違う。
                    .flex_shrink_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(CHROME_FONT_SIZE))
                    .text_color(gpui_filer_ui::theme_palette(cx).text)
                    // タイトルバーの中なので、カーソルは矢印のままにする。
                    // 開いている menu と hover を同じ濃さにして、開いたまま指を乗せても
                    // 一段変わらないようにする（menu の面と揃える）。
                    .when(active, |title| {
                        title.bg(gpui_filer_ui::theme_palette(cx).chrome_hover)
                    })
                    .hover(|title| title.bg(gpui_filer_ui::theme_palette(cx).chrome_hover))
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        if *hovered {
                            this.show_menu(menu, cx);
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                            if event.standard_click() {
                                this.show_menu(menu, cx);
                            }
                        }),
                    )
                    .child(title.label.clone()),
            );
        }
        let scrollbar = horizontal_scrollbar(
            "menu-scrollbar",
            &self.menu_scroll,
            self.menu_hovered,
            gpui_filer_ui::theme_palette(cx),
        );
        Some(
            div()
                .id("menu-bar-region")
                // 収まるときは内容幅、足りなければ縮んで中をスクロールする。
                .flex_shrink(1.0)
                .min_w_0()
                .h_full()
                // スクロールバーを重ねるため。
                .relative()
                .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                    if this.menu_hovered != *hovered {
                        this.menu_hovered = *hovered;
                        cx.notify();
                    }
                }))
                .child(row)
                .children(scrollbar)
                .into_any_element(),
        )
    }

    /// 開いている面を重ねる。メニューモードか、ポインタの位置の popup のときだけ。
    ///
    /// メニューバーはタイトルバーの中に描くので、覆うのはその下からにする。こうすると
    /// バーの hover、ウィンドウボタン、ドラッグ領域が menu を開いたままでも生きる。
    /// popup は今まで通り画面全体を覆う。
    pub(super) fn render_menu_overlay(
        &mut self,
        titles: &[MenuTitle],
        window: &Window,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let popup_at = match self.menu.open() {
            Some(menu::Open::Popup(_)) => self.menu_at,
            _ => None,
        };
        if popup_at.is_none() && !self.menu.bar_visible() {
            return None;
        }
        let top = if popup_at.is_some() {
            0.0
        } else {
            titlebar::HEIGHT
        };
        let mut overlay = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            // 外側のクリックで menu モードごと終わる。下の一覧へは届かせない。
            .child(
                div()
                    .absolute()
                    .top(px(top))
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .occlude()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _window, cx| this.leave_menu(cx)),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.replace_context_menu_from_backdrop(event.position, window, cx);
                        }),
                    ),
            );
        if let Some(panel) = self.render_menu_panel(titles, popup_at, window, viewport, cx) {
            overlay = overlay.child(panel);
        }
        Some(overlay)
    }

    pub(super) fn menu_row_height(&self, open: menu::Open) -> f32 {
        match open {
            menu::Open::Menu(menu::Menu::View) => VIEW_ROW_HEIGHT,
            menu::Open::Popup(menu::Popup::ReopenTabs | menu::Popup::Workspace) => {
                REOPEN_ROW_HEIGHT
            }
            menu::Open::Popup(menu::Popup::Item | menu::Popup::Background)
                if self
                    .menu_target
                    .as_ref()
                    .is_some_and(|target| target.touch_target) =>
            {
                TOUCH_CONTEXT_ROW_HEIGHT
            }
            _ => ROW_HEIGHT,
        }
    }

    pub(super) fn menu_panel_scroll(&self, open: menu::Open, child: bool) -> &ScrollHandle {
        if child {
            return &self.view_menu_scroll;
        }
        match open {
            menu::Open::Popup(menu::Popup::Item | menu::Popup::Background) => {
                &self.context_popup_scroll
            }
            menu::Open::Popup(menu::Popup::ReopenTabs) => &self.reopen_scroll,
            _ => &self.view_menu_root_scroll,
        }
    }

    /// dropdown の面。dropdown はタイトルの真下、popup はポインタの近くに置く。
    fn render_menu_panel(
        &mut self,
        titles: &[MenuTitle],
        popup_at: Option<gpui::Point<gpui::Pixels>>,
        window: &Window,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let open = self.menu.open()?;
        let bar_submenu = matches!(
            open,
            menu::Open::Menu(menu::Menu::View | menu::Menu::Updates)
        );
        let context_popup = matches!(
            open,
            menu::Open::Popup(menu::Popup::Item | menu::Popup::Background)
        );
        let submenu_root = bar_submenu || context_popup;
        let reopen_popup = open == menu::Open::Popup(menu::Popup::ReopenTabs);
        let workspace_popup = open == menu::Open::Popup(menu::Popup::Workspace);
        let (rows, separators) = if bar_submenu {
            (self.bar_submenu_root_rows(open, cx), Vec::new())
        } else if context_popup {
            let root = self.context_menu_root_rows(self.menu_entries(cx));
            (root.rows, root.separators)
        } else {
            (self.menu_rows(cx), Vec::new())
        };
        let child_open = submenu_root && self.menu.view_submenu().is_some();
        let child_rows = if child_open {
            self.menu_rows(cx)
        } else {
            Vec::new()
        };
        let view = self.menu_target.as_ref().and_then(|target| {
            let (pane, tab) = self
                .workspace
                .active()
                .pane_and_tab(target.tab, target.view?)?;
            (target.workspace == self.workspace.active
                && pane.id == target.pane
                && pane.directory().active_tab_id == target.tab)
                .then(|| tab.view.read(cx))
        });
        let focus = view.map(DirectoryListDemo::base_scopes);
        let hover = view
            .filter(|_| {
                open == menu::Open::Popup(menu::Popup::Item)
                    && self
                        .menu_target
                        .as_ref()
                        .is_some_and(|target| !target.touch_target)
                    && !focus
                        .as_ref()
                        .is_some_and(|scopes| scopes.contains(&input::scope::SECTION_FOCUS))
            })
            .map(DirectoryListDemo::item_path);
        let keymap = self.keymap_runtime.snapshot();
        let shortcut = |row: &menu::Row| {
            let menu::Row::Leaf(entry) = row else {
                return None;
            };
            keymap
                .file_view
                .key_for_action(
                    entry.action,
                    focus.as_deref()?,
                    hover.as_ref().map(|path| path.scopes()),
                )
                .map(shortcut_label)
        };
        let root_shortcuts = rows.iter().map(shortcut).collect::<Vec<_>>();
        let child_shortcuts = child_rows.iter().map(shortcut).collect::<Vec<_>>();
        let requested_root_width = menu_column_width(&rows, &root_shortcuts, window);
        let requested_child_width = menu_column_width(&child_rows, &child_shortcuts, window);
        let row_height = self.menu_row_height(open);
        let viewport_width = viewport.width.as_f32();
        let viewport_height = viewport.height.as_f32();
        let x_margin = viewport_margin(viewport_width);
        let y_margin = viewport_margin(viewport_height);
        let usable_width = viewport_width - 2.0 * x_margin;
        let bottom = viewport_height - y_margin;
        let minimum_top = if popup_at.is_some() {
            y_margin
        } else {
            titlebar::HEIGHT
        };
        let available_height = (bottom - minimum_top).max(0.0);
        if usable_width <= POPUP_PANEL_CHROME_HEIGHT
            || available_height <= POPUP_PANEL_CHROME_HEIGHT
        {
            return None;
        }
        let root_height = (menu_row_top(rows.len(), row_height, &separators)
            + POPUP_PANEL_CHROME_HEIGHT)
            .min(available_height);
        let (left, top, width, geometry) = if let Some(at) = popup_at {
            let requested_width = if reopen_popup {
                420.0
            } else {
                requested_root_width
            };
            // 分岐を開いたときも親の位置と幅を保ち、隣に子列を置く余地を残す。
            let has_branch = rows
                .iter()
                .any(|row| matches!(row, menu::Row::Branch { .. }));
            let width_limit = if has_branch {
                usable_width / 2.0
            } else {
                usable_width
            };
            let width = requested_width.min(width_limit);
            let left = popup_position(at.x.as_f32(), width, viewport_width);
            let top = popup_position(at.y.as_f32(), root_height, viewport_height);
            let geometry = child_open.then(|| {
                context_submenu_geometry(left, viewport_width, width, requested_child_width)
            });
            (left, top, width, geometry)
        } else {
            let menu = self.menu.open_menu()?;
            let index = titles.iter().position(|item| item.menu == menu)?;
            // メニューバーの横スクロール後も、見えているタイトルを基準にする。
            let anchor = title_x(titles, index) + self.menu_scroll.offset().x.as_f32();
            if bar_submenu {
                let geometry = view_menu_geometry(
                    anchor,
                    viewport_width,
                    requested_root_width,
                    requested_child_width,
                );
                (
                    geometry.root_left,
                    minimum_top,
                    geometry.root_column_width,
                    Some(geometry),
                )
            } else {
                let width = requested_root_width.min(usable_width);
                let left =
                    anchor.clamp(x_margin, (viewport_width - x_margin - width).max(x_margin));
                (left, minimum_top, width, None)
            }
        };
        let palette = gpui_filer_ui::theme_palette(cx);
        let root_scroll = self.menu_panel_scroll(open, false).clone();
        let mut row_panel = div()
            .id("shell-menu-rows")
            .max_h(px(root_height - POPUP_PANEL_CHROME_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&root_scroll);
        let highlighted = if child_open {
            Some(self.menu.parent_highlight_index())
        } else {
            self.menu.active_highlight()
        };
        for (index, (row, shortcut)) in rows.into_iter().zip(root_shortcuts).enumerate() {
            if separators.contains(&index) {
                row_panel = row_panel.child(
                    div()
                        .h(px(MENU_SEPARATOR_HEIGHT))
                        .px(px(MENU_ROW_PADDING_X))
                        .flex()
                        .items_center()
                        .child(div().h(px(1.0)).w_full().bg(popup_border(palette))),
                );
            }
            row_panel = row_panel.child(self.render_menu_row(
                index,
                row,
                highlighted,
                row_height,
                (reopen_popup || submenu_root || workspace_popup).then_some(index),
                submenu_root,
                shortcut,
                cx,
            ));
        }
        let panel = popup_panel(palette)
            .top(px(top))
            .left(px(left))
            .w(px(width))
            .child(row_panel);
        let mut panels = div().absolute().inset_0().child(panel);
        if let Some(geometry) = geometry.filter(|_| child_open) {
            let child_height = (menu_row_top(child_rows.len(), row_height, &[])
                + POPUP_PANEL_CHROME_HEIGHT)
                .min(available_height);
            // どちらの面も overlay 基準で置き、両方の行の内側が揃うようにする。
            let child_anchor = top
                + menu_row_top(self.menu.parent_highlight_index(), row_height, &separators)
                + root_scroll.offset().y.as_f32();
            let child_top =
                child_anchor.clamp(minimum_top, (bottom - child_height).max(minimum_top));
            let child_left = match geometry.child_side {
                ChildSide::Right => left + width,
                ChildSide::Left => left - geometry.child_column_width,
            };
            let mut child_rows_panel = div()
                .id("shell-view-submenu-rows")
                .max_h(px(child_height - POPUP_PANEL_CHROME_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(self.menu_panel_scroll(open, true));
            for (index, (row, shortcut)) in child_rows.into_iter().zip(child_shortcuts).enumerate()
            {
                child_rows_panel = child_rows_panel.child(self.render_menu_row(
                    index,
                    row,
                    self.menu.active_highlight(),
                    row_height,
                    Some(index),
                    false,
                    shortcut,
                    cx,
                ));
            }
            panels = panels.child(
                popup_panel(palette)
                    .id("shell-view-submenu")
                    .top(px(child_top))
                    .left(px(child_left))
                    .w(px(geometry.child_column_width))
                    .child(child_rows_panel),
            );
        }
        Some(panels)
    }

    /// dropdown の 1 行。
    fn render_menu_row(
        &self,
        index: usize,
        row: menu::Row,
        highlighted: Option<usize>,
        row_height: f32,
        row_index: Option<usize>,
        root_view: bool,
        shortcut: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = row.label().to_owned();
        let enabled = match &row {
            menu::Row::Leaf(entry) => entry.enabled,
            menu::Row::Info { .. } | menu::Row::Theme { .. } | menu::Row::Branch { .. } => true,
        };
        let action = match &row {
            menu::Row::Leaf(entry) => Some(entry.action),
            _ => None,
        };
        let branch = matches!(row, menu::Row::Branch { .. });
        let info = matches!(row, menu::Row::Info { .. });
        let icon = if row.checked() {
            Some(IconId::Check)
        } else if matches!(
            row,
            menu::Row::Branch {
                submenu: menu::ViewSubmenu::Creation,
                ..
            }
        ) {
            Some(IconId::Add)
        } else {
            action.and_then(action_icon)
        };
        let palette = gpui_filer_ui::theme_palette(cx);
        let text_color = if !enabled || info {
            palette.text_muted
        } else {
            palette.text
        };
        let highlight_color = if root_view && self.menu.view_submenu().is_some() {
            palette.chrome_hover_strong
        } else {
            palette.chrome_hover
        };
        let highlightable = enabled;
        div()
            .id(("menu-row", index))
            .h(px(row_height))
            .px(px(MENU_ROW_PADDING_X))
            .flex()
            .items_center()
            .gap(px(MENU_ROW_GAP))
            .rounded(px(6.0))
            .text_color(text_color)
            // 選ばれている行は一つだけ。使えない行でも印を出す。
            // 印が出ないと、キーで動かした先が読めない。
            .when(highlighted == Some(index), |element| {
                element.bg(highlight_color)
            })
            .child(
                div()
                    .w(px(MENU_ICON_SIZE))
                    .flex_shrink_0()
                    .children(icon.map(|icon| {
                        Icon::new(icon)
                            .size(px(MENU_ICON_SIZE))
                            .color(text_color.into())
                    })),
            )
            .child(div().min_w_0().flex_1().truncate().child(label))
            .children(shortcut.map(|label| {
                div()
                    .max_w(gpui::relative(0.45))
                    .text_size(px(SHORTCUT_FONT_SIZE))
                    .text_color(palette.text_muted)
                    .truncate()
                    .child(label)
            }))
            .when(branch, |element| {
                element.child(
                    div().flex_shrink_0().child(
                        Icon::new(IconId::Expand)
                            .size(px(MENU_ICON_SIZE))
                            .color(palette.text.into()),
                    ),
                )
            })
            .when(highlightable, |element| {
                element
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            if root_view {
                                this.hover_view_root_row(index, cx);
                            } else {
                                this.hover_menu_row(index, cx);
                            }
                        } else {
                            this.clear_menu_hover(index, cx);
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            cx.stop_propagation();
                            if event.standard_click() {
                                if let Some(action) = action {
                                    this.run_menu_command_at(action, row_index, window, cx);
                                } else if branch {
                                    this.open_view_submenu_at(index, cx);
                                } else if let menu::Row::Theme { basename, .. } = &row {
                                    if this.theme_names.iter().any(|name| name == basename) {
                                        this.leave_menu(cx);
                                        this.select_theme(basename.clone(), cx);
                                    }
                                }
                            }
                        }),
                    )
            })
    }
}

/// タイトルの幅。GPUI と同じ文字整形で測る。
///
/// 文字列に固定幅を仮定しない。翻訳でラベルが伸びても、タイトルと dropdown が一緒に動く。
fn title_width(label: &str, window: &Window) -> f32 {
    (text_width(label, CHROME_FONT_SIZE, window) + TITLE_PADDING_X * 2.0).max(TITLE_MIN_WIDTH)
}

fn text_width(label: &str, font_size: f32, window: &Window) -> f32 {
    let style = window.text_style();
    let shaped = window.text_system().shape_text(
        label.to_string().into(),
        px(font_size),
        &[style.to_run(label.len())],
        None,
        None,
    );
    shaped
        .ok()
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.width().as_f32())
                .fold(0.0, f32::max)
        })
        .unwrap_or(0.0)
}

/// dropdown の左端。バーの左端（ハンバーガーの右）から、その menu までの幅を足す。
fn title_x(titles: &[MenuTitle], index: usize) -> f32 {
    titlebar::HAMBURGER_WIDTH
        + titles
            .iter()
            .take(index)
            .map(|title| title.width)
            .sum::<f32>()
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn narrow_view_menu_keeps_both_columns_inside_viewport() {
        let geometry = view_menu_geometry(64.0, 320.0, 176.0, 176.0);
        assert_eq!(geometry.root_column_width, 152.0);
        assert_eq!(geometry.child_column_width, 152.0);
        assert_eq!(geometry.root_left, 8.0);
        assert_eq!(geometry.child_side, ChildSide::Right);
        assert!(
            geometry.root_left + geometry.root_column_width + geometry.child_column_width <= 312.0
        );
    }

    #[test]
    fn wide_view_menu_uses_available_side_without_moving_root() {
        let geometry = view_menu_geometry(64.0, 1024.0, 176.0, 176.0);
        assert_eq!(geometry.root_column_width, PANEL_WIDTH);
        assert_eq!(geometry.child_column_width, PANEL_WIDTH);
        assert_eq!(geometry.root_left, 64.0);
        assert_eq!(geometry.child_side, ChildSide::Right);

        let right_edge = view_menu_geometry(900.0, 1024.0, 176.0, 176.0);
        assert_eq!(right_edge.root_left, 840.0);
        assert_eq!(right_edge.child_side, ChildSide::Left);
    }

    #[test]
    fn wide_labels_keep_asymmetric_menu_columns_inside_viewport() {
        let left_anchor = view_menu_geometry(64.0, 1024.0, 260.0, 320.0);
        assert_eq!(left_anchor.root_column_width, 260.0);
        assert_eq!(left_anchor.child_column_width, 320.0);
        assert_eq!(left_anchor.root_left, 64.0);
        assert_eq!(left_anchor.child_side, ChildSide::Right);

        let right_anchor = view_menu_geometry(900.0, 1024.0, 260.0, 320.0);
        assert_eq!(right_anchor.root_left, 756.0);
        assert_eq!(right_anchor.child_side, ChildSide::Left);
        assert!(right_anchor.root_left - right_anchor.child_column_width >= 8.0);
        assert!(right_anchor.root_left + right_anchor.root_column_width <= 1016.0);
    }

    #[test]
    fn child_width_changes_do_not_move_root_or_switch_sides() {
        for (anchor, viewport_width) in [(64.0, 1024.0), (250.0, 600.0)] {
            let short_child = view_menu_geometry(anchor, viewport_width, 176.0, 120.0);
            let wide_child = view_menu_geometry(anchor, viewport_width, 176.0, 900.0);

            assert_eq!(short_child.root_left, wide_child.root_left);
            assert_eq!(short_child.root_column_width, wide_child.root_column_width);
            assert_eq!(short_child.child_side, wide_child.child_side);
            assert_eq!(short_child.child_column_width, 120.0);
            assert!(wide_child.child_column_width >= short_child.child_column_width);

            let (left, right) = match wide_child.child_side {
                ChildSide::Left => (
                    wide_child.root_left - wide_child.child_column_width,
                    wide_child.root_left + wide_child.root_column_width,
                ),
                ChildSide::Right => (
                    wide_child.root_left,
                    wide_child.root_left
                        + wide_child.root_column_width
                        + wide_child.child_column_width,
                ),
            };
            assert!(left >= 8.0);
            assert!(right <= viewport_width - 8.0);
        }
    }

    #[test]
    fn view_menu_scroll_reveals_rows_and_clamps_at_content_end() {
        assert_eq!(reveal_scroll_offset(0.0, 0.0, 44.0, 192.0, 220.0), 0.0);
        assert_eq!(reveal_scroll_offset(0.0, 176.0, 44.0, 192.0, 220.0), 28.0);
        assert_eq!(reveal_scroll_offset(28.0, 132.0, 44.0, 192.0, 220.0), 28.0);
        assert_eq!(reveal_scroll_offset(28.0, 0.0, 44.0, 192.0, 220.0), 0.0);
    }

    #[test]
    fn context_popup_offsets_flips_and_clamps_with_the_same_panel_extent() {
        assert_eq!(popup_position(120.0, 176.0, 600.0), 132.0);
        assert_eq!(popup_position(590.0, 176.0, 600.0), 402.0);
        assert_eq!(popup_position(200.0, 384.0, 400.0), 8.0);
        let child = context_submenu_geometry(402.0, 600.0, 176.0, 220.0);
        assert_eq!(child.root_left, 402.0);
        assert_eq!(child.root_column_width, 176.0);
        assert_eq!(child.child_column_width, 220.0);
        assert_eq!(child.child_side, ChildSide::Left);
    }

    #[test]
    fn separators_count_toward_submenu_anchors_and_keyboard_reveal() {
        let row_height = TOUCH_CONTEXT_ROW_HEIGHT;
        let separators = [2, 5];
        assert_eq!(menu_row_top(1, row_height, &separators), 44.0);
        assert_eq!(menu_row_top(2, row_height, &separators), 97.0);
        let last_row = menu_row_top(9, row_height, &separators);
        let content_height = menu_row_top(10, row_height, &separators);
        assert_eq!(last_row, 414.0);
        assert_eq!(content_height, 458.0);
        let viewport_height = 400.0 - 2.0 * MENU_VIEWPORT_MARGIN - POPUP_PANEL_CHROME_HEIGHT;
        let top = reveal_scroll_offset(0.0, last_row, row_height, viewport_height, content_height);
        assert_eq!(top, content_height - viewport_height);
        assert!(last_row >= top);
        assert!(last_row + row_height <= top + viewport_height);
    }
}
