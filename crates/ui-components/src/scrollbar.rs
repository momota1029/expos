//! 下端に重ねる横スクロールバー。
//!
//! パンくずバーとタイトルバーのメニューバーで共有する。どちらも「1 行のバーが横に溢れる」
//! 形なので、バーの下端に細い帯を重ねる。パンくずの hit 領域（44px）やタイトルの行いっぱいの
//! hit 領域と重なっても、**帯は `occlude` しない**ので下の要素のクリックは生きる
//! （帯自身はクリックを持たず、つまみだけが握る）。
//!
//! つまみのどこを掴んだかは drag の payload（`Cell<f32>`）が持つ。呼び出し側が state を
//! 持たなくても drag が成立するので、`RenderOnce` の component からも使える。
//!
//! つまみを出すかどうかは呼び出し側が決める。component は hover 状態を持てないので、
//! 解決済みの状態を受け取る（他の component と同じ境界）。

use crate::ThemePalette;
use gpui::{
    AppContext as _, InteractiveElement as _, ParentElement as _, Pixels, Point, ScrollHandle,
    Stateful, StatefulInteractiveElement as _, Styled as _, div, px,
};
use std::cell::Cell;

/// 帯の高さ。1 行のバーの下端に置ける細さにする。
pub const STRIP_HEIGHT: f32 = 4.0;
/// つまみの最小幅。
const MIN_THUMB_WIDTH: f32 = 40.0;

/// 横スクロールバー。入る余地が無ければ `None`。
///
/// `id` は同じ window に複数を置けるように呼び出し側が決める。
/// `visible` が false のときも帯は描く（drag の受け口と hover の検出を残す）が、
/// つまみは描かない。
pub fn horizontal_scrollbar(
    id: &'static str,
    scroll: &ScrollHandle,
    visible: bool,
    palette: ThemePalette,
) -> Option<Stateful<gpui::Div>> {
    let width = scroll.bounds().size.width.as_f32();
    let max_scroll = scroll.max_offset().x.as_f32();
    if width <= 0.0 || max_scroll <= 0.0 {
        return None;
    }
    let content = width + max_scroll;
    let thumb_width = (width * width / content).max(MIN_THUMB_WIDTH).min(width);
    let travel = width - thumb_width;
    // offset.x は右へ動かすほど負になる。
    let left = (-scroll.offset().x.as_f32() / max_scroll).clamp(0.0, 1.0) * travel;

    let drag_scroll = scroll.clone();
    let mut strip = div()
        .id(id)
        .absolute()
        .left_0()
        .bottom_0()
        .w_full()
        .h(px(STRIP_HEIGHT))
        .on_drag_move(move |event: &gpui::DragMoveEvent<Cell<f32>>, window, cx| {
            if travel <= 0.0 {
                return;
            }
            let grab = event.drag(cx).get();
            let start = drag_scroll.bounds().origin.x.as_f32();
            let position = (event.event.position.x.as_f32() - start - grab).clamp(0.0, travel);
            drag_scroll.set_offset(gpui::point(px(-position / travel * max_scroll), px(0.0)));
            window.refresh();
            cx.stop_propagation();
        });

    if visible {
        strip = strip.child(
            div()
                .id((id, 1usize))
                .absolute()
                .left(px(left))
                .top_0()
                .w(px(thumb_width))
                .h(px(STRIP_HEIGHT))
                .rounded(px(STRIP_HEIGHT / 2.0))
                .bg(palette.scrollbar_thumb)
                .hover(|style| style.bg(palette.scrollbar_thumb_hover))
                // つまみのクリックは下のバーへ通さない。
                .on_click(|_, _, cx| cx.stop_propagation())
                .on_drag(
                    Cell::new(0.0f32),
                    move |grab: &Cell<f32>, offset: Point<Pixels>, _window, cx| {
                        grab.set(offset.x.as_f32());
                        cx.new(|_| gpui::Empty)
                    },
                ),
        );
    }
    Some(strip)
}
