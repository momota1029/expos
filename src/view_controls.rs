use gpui::{
    Context, Hsla, InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_filer_ui::RADIUS_SM;

use super::{
    DirectoryEvent, DirectoryListDemo, GroupBy, ICON_SCALE_MAX, ICON_SCALE_MIN, ICON_SCALE_STEP,
    Message, ViewLayout,
};

impl DirectoryListDemo {
    pub(super) fn render_layout_switch(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = gpui_filer_ui::theme_palette(cx);
        let mut controls = div().flex().flex_wrap().gap(px(2.0));
        for (id, message, layout) in [
            ("view-rows", Message::LayoutRows, ViewLayout::Rows),
            ("view-tiles", Message::LayoutTiles, ViewLayout::Tiles),
            ("view-icons", Message::LayoutIcons, ViewLayout::Icons),
        ] {
            let label = self.translator.text(message).into_owned();
            let active = self.layout == layout;
            controls = controls.child(
                div()
                    .id(id)
                    .min_w(px(44.0))
                    .min_h(px(44.0))
                    .max_w(px(168.0))
                    .flex_shrink_0()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(RADIUS_SM))
                    .when(active, |button| button.bg(palette.accent))
                    // 選択中はアクセントのまま明るくする。灰色へ落とすと選択が読めない。
                    .hover(move |style| {
                        style.bg(if active {
                            palette.accent_hover
                        } else {
                            palette.chrome_hover
                        })
                    })
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .whitespace_normal()
                            .text_center()
                            .child(label),
                    )
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                            if event.standard_click() {
                                this.set_layout(layout, cx);
                            }
                        }),
                    ),
            );
        }
        controls
    }

    pub(super) fn render_group_switch(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let palette = gpui_filer_ui::theme_palette(cx);
        let active = self.group_by != GroupBy::None;
        let label = match self.group_by {
            GroupBy::None => self.translator.text(Message::GroupNone),
            GroupBy::ModifiedDate => self.translator.text(Message::GroupByDate),
            GroupBy::Kind => self.translator.text(Message::GroupByKind),
        };
        div()
            .id("group-modified-date")
            .min_h(px(44.0))
            .max_w(px(168.0))
            .px(px(10.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(RADIUS_SM))
            .when(active, |button| button.bg(palette.accent))
            .hover(move |style| {
                style.bg(if active {
                    palette.accent_hover
                } else {
                    palette.chrome_hover
                })
            })
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .whitespace_normal()
                    .text_center()
                    .child(label.into_owned()),
            )
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _window, cx| {
                if event.standard_click() {
                    this.cycle_grouping(cx);
                }
            }))
    }

    /// タイル・アイコンビューのアイコンサイズを段階的に変える −/+ コントロール。
    pub(super) fn render_icon_scale(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = gpui_filer_ui::theme_palette(cx);
        let can_shrink = self.icon_scale > ICON_SCALE_MIN + 0.01;
        let can_grow = self.icon_scale < ICON_SCALE_MAX - 0.01;
        let dim: Hsla = rgb(0x555555).into();
        div()
            .flex()
            .items_center()
            .child(
                div()
                    .id("icon-smaller")
                    .w(px(44.0))
                    .h(px(44.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .when(can_shrink, |b| b.hover(|s| s.bg(palette.border)))
                    .when(!can_shrink, |b| b.text_color(dim))
                    .child("-")
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                        if event.standard_click() && this.icon_scale > ICON_SCALE_MIN + 0.01 {
                            this.icon_scale =
                                (this.icon_scale - ICON_SCALE_STEP).max(ICON_SCALE_MIN);
                            this.layout_width = 0.0;
                            cx.emit(DirectoryEvent::StateChanged);
                            cx.notify();
                        }
                    })),
            )
            .child(
                div()
                    .id("icon-larger")
                    .w(px(44.0))
                    .h(px(44.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .when(can_grow, |b| b.hover(|s| s.bg(palette.border)))
                    .when(!can_grow, |b| b.text_color(dim))
                    .child("+")
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                        if event.standard_click() && this.icon_scale < ICON_SCALE_MAX - 0.01 {
                            this.icon_scale =
                                (this.icon_scale + ICON_SCALE_STEP).min(ICON_SCALE_MAX);
                            this.layout_width = 0.0;
                            cx.emit(DirectoryEvent::StateChanged);
                            cx.notify();
                        }
                    })),
            )
    }
}
