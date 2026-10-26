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
const MENU_ROW_CHECK_WIDTH: f32 = 16.0;
const MENU_ROW_BRANCH_ICON_WIDTH: f32 = 10.0;
/// dropdown の 1 行の高さ。
pub(super) const ROW_HEIGHT: f32 = 26.0;
/// Touch-invoked context-menu actions need the same minimum hit height as other touch controls.
pub(super) const TOUCH_CONTEXT_ROW_HEIGHT: f32 = 44.0;
pub(super) const VIEW_ROW_HEIGHT: f32 = 44.0;
pub(super) const REOPEN_ROW_HEIGHT: f32 = 44.0;
const POPUP_PANEL_CHROME_HEIGHT: f32 = 10.0;
/// メニューバーのタイトルの左右の余白。
const TITLE_PADDING_X: f32 = 12.0;
/// メニューバーのタイトルの最小幅。タッチの目安（44）を下回らない。
const TITLE_MIN_WIDTH: f32 = 44.0;
const MENU_VIEWPORT_MARGIN: f32 = 8.0;

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
    let margin = MENU_VIEWPORT_MARGIN.min((viewport_width / 4.0).max(0.0));
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

fn menu_column_width(rows: &[menu::Row], window: &Window) -> f32 {
    rows.iter()
        .map(|row| {
            let branch_width = if matches!(row, menu::Row::Branch { .. }) {
                MENU_ROW_GAP + MENU_ROW_BRANCH_ICON_WIDTH
            } else {
                0.0
            };
            title_width(row.label(), window)
                + MENU_ROW_PADDING_X * 2.0
                + MENU_ROW_GAP
                + MENU_ROW_CHECK_WIDTH
                + branch_width
        })
        .fold(PANEL_WIDTH, f32::max)
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
    let next_top = if row_top < current_top {
        row_top
    } else if row_top + row_height > current_top + viewport_height {
        row_top + row_height - viewport_height
    } else {
        current_top
    };
    next_top.clamp(0.0, content_height - viewport_height)
}

fn context_popup_top(anchor_top: f32, viewport_height: f32, row_content_height: f32) -> f32 {
    let panel_height = (row_content_height + POPUP_PANEL_CHROME_HEIGHT).min(viewport_height);
    anchor_top.clamp(0.0, (viewport_height - panel_height).max(0.0))
}

fn context_submenu_geometry(
    root_left: f32,
    viewport_width: f32,
    requested_root_width: f32,
    requested_child_width: f32,
) -> ViewMenuGeometry {
    let margin = MENU_VIEWPORT_MARGIN.min((viewport_width / 4.0).max(0.0));
    let usable_width = (viewport_width - margin * 2.0).max(0.0);
    let root_column_width = requested_root_width.min(usable_width / 2.0);
    let left_space = (root_left - margin).max(0.0);
    let right_space = (viewport_width - margin - root_left - root_column_width).max(0.0);
    let child_side = if right_space >= left_space {
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
        root_column_width,
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
    /// メニューバーに並べるタイトル。幅を測るのはここ 1 回だけ。
    pub(super) fn menu_titles(&self, window: &Window) -> Vec<MenuTitle> {
        menu::Menu::ALL
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

    /// dropdown の面。dropdown はタイトルの真下、popup はポインタの位置。
    fn render_menu_panel(
        &mut self,
        titles: &[MenuTitle],
        popup_at: Option<gpui::Point<gpui::Pixels>>,
        window: &Window,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let view_menu = self.menu.open() == Some(menu::Open::Menu(menu::Menu::View));
        let context_popup = matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Item | menu::Popup::Background
            ))
        );
        let submenu_root = view_menu || context_popup;
        let child_rows = self.menu_rows(cx);
        let rows = if view_menu {
            self.view_menu_root_rows(cx)
        } else if context_popup {
            self.context_menu_root_rows(self.menu_entries(cx))
        } else {
            child_rows.clone()
        };
        let requested_root_width = menu_column_width(&rows, window);
        let requested_child_width = menu_column_width(&child_rows, window);
        let touch_context_menu = matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Item | menu::Popup::Background | menu::Popup::Workspace
            ))
        ) && self
            .menu_target
            .as_ref()
            .is_some_and(|target| target.touch_target);
        let tab_popup = matches!(
            self.menu.open(),
            Some(menu::Open::Popup(
                menu::Popup::Tab | menu::Popup::ReopenTabs | menu::Popup::Workspace
            ))
        );
        let reopen_popup = self.menu.open() == Some(menu::Open::Popup(menu::Popup::ReopenTabs));
        let workspace_popup = self.menu.open() == Some(menu::Open::Popup(menu::Popup::Workspace));
        let (left, top) = match popup_at {
            Some(at) if context_popup => {
                let row_height = if touch_context_menu {
                    TOUCH_CONTEXT_ROW_HEIGHT
                } else {
                    ROW_HEIGHT
                };
                let content_height = rows.len() as f32 * row_height;
                let panel_width = requested_root_width;
                let max_left = (viewport.width.as_f32() - panel_width).max(0.0);
                (
                    at.x.as_f32().clamp(0.0, max_left),
                    context_popup_top(at.y.as_f32(), viewport.height.as_f32(), content_height),
                )
            }
            Some(at) if tab_popup => {
                // Shell root は overflow を clip するため、タブ menu は既知の寸法で
                // viewport 内へ寄せる。余白があれば pointer anchor はそのまま保つ。
                let panel_width = if reopen_popup {
                    (viewport.width - px(16.0)).min(px(420.0)).max(px(0.0))
                } else {
                    px(PANEL_WIDTH)
                };
                let row_height = if reopen_popup {
                    REOPEN_ROW_HEIGHT
                } else if workspace_popup {
                    REOPEN_ROW_HEIGHT
                } else if view_menu {
                    VIEW_ROW_HEIGHT
                } else if touch_context_menu {
                    TOUCH_CONTEXT_ROW_HEIGHT
                } else {
                    ROW_HEIGHT
                };
                let panel_height = px(rows.len() as f32 * row_height + 10.0);
                let max_left = (viewport.width - panel_width).max(px(0.0));
                let max_top = (viewport.height - panel_height).max(px(0.0));
                (
                    at.x.min(max_left).max(px(0.0)).as_f32(),
                    at.y.min(max_top).max(px(0.0)).as_f32(),
                )
            }
            Some(at) => (at.x.as_f32(), at.y.as_f32()),
            None => {
                let menu = self.menu.open_menu()?;
                let index = menu::Menu::ALL.iter().position(|item| *item == menu)?;
                // バーが横スクロールしていれば、見えている左端もその分ずれる。
                let anchor = title_x(titles, index) + self.menu_scroll.offset().x.as_f32();
                let left = if view_menu {
                    view_menu_geometry(
                        anchor,
                        viewport.width.as_f32(),
                        requested_root_width,
                        requested_child_width,
                    )
                    .root_left
                } else {
                    anchor.max(0.0)
                };
                (left, titlebar::HEIGHT)
            }
        };
        let highlighted = if submenu_root && self.menu.view_submenu().is_some() {
            Some(self.menu.parent_highlight_index())
        } else {
            self.menu.active_highlight()
        };
        let root_row_height = if view_menu {
            VIEW_ROW_HEIGHT
        } else if touch_context_menu {
            TOUCH_CONTEXT_ROW_HEIGHT
        } else if reopen_popup || workspace_popup {
            REOPEN_ROW_HEIGHT
        } else {
            ROW_HEIGHT
        };
        let view_geometry = submenu_root.then(|| {
            if context_popup {
                context_submenu_geometry(
                    left,
                    viewport.width.as_f32(),
                    requested_root_width,
                    requested_child_width,
                )
            } else {
                view_menu_geometry(
                    left,
                    viewport.width.as_f32(),
                    requested_root_width,
                    requested_child_width,
                )
            }
        });
        // コンテキストメニューの親列は pointer anchor に固定し、子列を開いても動かさない。
        let root_width = view_geometry.map_or(PANEL_WIDTH, |geometry| geometry.root_column_width);
        let available_view_height = if context_popup {
            (viewport.height.as_f32() - 10.0).max(VIEW_ROW_HEIGHT)
        } else {
            (viewport.height.as_f32() - titlebar::HEIGHT - MENU_VIEWPORT_MARGIN - 10.0)
                .max(VIEW_ROW_HEIGHT)
        };
        let mut panel = div()
            .absolute()
            .top(px(top))
            .left(px(left))
            .when(reopen_popup, |element| {
                let width = (viewport.width - px(16.0)).min(px(420.0)).max(px(0.0));
                element.w(width).min_w(width).max_w(width)
            })
            .when(!reopen_popup, |element| {
                if submenu_root {
                    element
                        .w(px(root_width))
                        .min_w(px(root_width))
                        .max_w(px(root_width))
                } else {
                    element.min_w(px(PANEL_WIDTH))
                }
            })
            .py(px(4.0))
            .flex()
            .flex_col()
            .text_size(px(CHROME_FONT_SIZE))
            .rounded(px(RADIUS_SM))
            .border_1()
            .border_color(gpui_filer_ui::theme_palette(cx).border)
            .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
            .occlude();
        let mut row_panel = div().id("shell-menu-rows");
        if context_popup {
            row_panel = row_panel
                .max_h((viewport.height - px(POPUP_PANEL_CHROME_HEIGHT)).max(px(0.0)))
                .overflow_y_scroll()
                .track_scroll(&self.context_popup_scroll);
        } else if reopen_popup {
            row_panel = row_panel
                .max_h((viewport.height - px(24.0)).max(px(0.0)))
                .overflow_y_scroll()
                .track_scroll(&self.reopen_scroll);
        } else if view_menu {
            row_panel = row_panel
                .max_h(px(available_view_height))
                .overflow_y_scroll()
                .track_scroll(&self.view_menu_root_scroll);
        }
        for (index, row) in rows.into_iter().enumerate() {
            row_panel = row_panel.child(self.render_menu_row(
                index,
                row,
                highlighted,
                root_row_height,
                (reopen_popup || submenu_root || workspace_popup).then_some(index),
                submenu_root,
                cx,
            ));
        }
        panel = panel.child(row_panel);
        if submenu_root && self.menu.view_submenu().is_some() {
            let geometry = view_geometry.expect("View menu geometry is present");
            let child_width = px(geometry.child_column_width);
            let root_scroll = if view_menu {
                self.view_menu_root_scroll.offset().y.as_f32()
            } else {
                self.context_popup_scroll.offset().y.as_f32()
            };
            let child_row_height = if view_menu {
                VIEW_ROW_HEIGHT
            } else if touch_context_menu {
                TOUCH_CONTEXT_ROW_HEIGHT
            } else {
                ROW_HEIGHT
            };
            let child_height = (child_rows.len() as f32 * child_row_height + 10.0)
                .min(available_view_height + 10.0);
            let child_anchor = top
                + 4.0
                + self.menu.parent_highlight_index() as f32 * child_row_height
                + root_scroll;
            let minimum_child_top = if context_popup { 0.0 } else { titlebar::HEIGHT };
            let child_top = child_anchor.clamp(
                minimum_child_top,
                (viewport.height.as_f32() - child_height).max(minimum_child_top),
            );
            let left_delta = match geometry.child_side {
                ChildSide::Right => geometry.root_column_width,
                ChildSide::Left => -geometry.child_column_width,
            };
            let mut child_panel = div()
                .id("shell-view-submenu")
                .absolute()
                .top(px(child_top - top))
                .left(px(left_delta))
                .w(child_width)
                .min_w(child_width)
                .max_w(child_width)
                .py(px(4.0))
                .flex()
                .flex_col()
                .text_size(px(CHROME_FONT_SIZE))
                .rounded(px(RADIUS_SM))
                .border_1()
                .border_color(gpui_filer_ui::theme_palette(cx).border)
                .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                .occlude();
            let mut child_rows_panel = div().id("shell-view-submenu-rows");
            child_rows_panel = child_rows_panel
                .max_h(px(available_view_height))
                .overflow_y_scroll()
                .track_scroll(&self.view_menu_scroll);
            for (index, row) in child_rows.into_iter().enumerate() {
                child_rows_panel = child_rows_panel.child(self.render_menu_row(
                    index,
                    row,
                    self.menu.active_highlight(),
                    child_row_height,
                    Some(index),
                    false,
                    cx,
                ));
            }
            child_panel = child_panel.child(child_rows_panel);
            panel = panel.child(child_panel);
        }
        Some(panel)
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
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = row.label().to_owned();
        let enabled = match &row {
            menu::Row::Leaf(entry) => entry.enabled,
            menu::Row::Info { .. } | menu::Row::Theme { .. } | menu::Row::Branch { .. } => true,
        };
        let checked = row.checked();
        let action = match &row {
            menu::Row::Leaf(entry) => Some(entry.action),
            _ => None,
        };
        let branch = matches!(row, menu::Row::Branch { .. });
        let highlightable = enabled;
        div()
            .id(("menu-row", index))
            .h(px(row_height))
            .px(px(MENU_ROW_PADDING_X))
            .flex()
            .items_center()
            .gap(px(MENU_ROW_GAP))
            .text_color(gpui_filer_ui::theme_palette(cx).text)
            // 選ばれている行は一つだけ。使えない行でも印を出す。
            // 印が出ないと、キーで動かした先が読めない。
            .when(highlighted == Some(index), |element| {
                element.bg(gpui_filer_ui::theme_palette(cx).chrome_hover)
            })
            .when(!enabled, |element| {
                element.text_color(gpui_filer_ui::theme_palette(cx).text_muted)
            })
            .child(if checked {
                Icon::new(IconId::Check).size(px(16.0)).into_any_element()
            } else {
                div().w(px(16.0)).flex_shrink_0().into_any_element()
            })
            .child(div().min_w_0().flex_1().truncate().child(label))
            .when(branch, |element| {
                element.child(
                    div().flex_shrink_0().child(
                        Icon::new(IconId::Expand)
                            .size(px(MENU_ROW_BRANCH_ICON_WIDTH))
                            .color(gpui_filer_ui::theme_palette(cx).text_muted.into()),
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
    let style = window.text_style();
    let shaped = window.text_system().shape_text(
        label.to_string().into(),
        px(CHROME_FONT_SIZE),
        &[style.to_run(label.len())],
        None,
        None,
    );
    let text = shaped
        .ok()
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.width().as_f32())
                .fold(0.0, f32::max)
        })
        .unwrap_or(0.0);
    (text + TITLE_PADDING_X * 2.0).max(TITLE_MIN_WIDTH)
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
    fn context_popup_clamps_near_bottom_and_scroll_reveals_last_touch_row() {
        let content_height = 10.0 * TOUCH_CONTEXT_ROW_HEIGHT;
        assert_eq!(context_popup_top(390.0, 400.0, content_height), 0.0);
        let viewport_height = 300.0;
        let top = reveal_scroll_offset(
            0.0,
            9.0 * TOUCH_CONTEXT_ROW_HEIGHT,
            TOUCH_CONTEXT_ROW_HEIGHT,
            viewport_height,
            content_height,
        );
        assert_eq!(top, content_height - viewport_height);
        assert!(9.0 * TOUCH_CONTEXT_ROW_HEIGHT >= top);
        assert!(10.0 * TOUCH_CONTEXT_ROW_HEIGHT <= top + viewport_height);
    }
}
