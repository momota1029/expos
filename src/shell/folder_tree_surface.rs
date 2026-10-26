use super::*;
use crate::folder_tree_menu;
use gpui::{App, Bounds, Pixels, Window, px};
use gpui_filer_resources::IconId;
use gpui_filer_virtual_list::{RowHeight, RowHeights};
use std::ops::Range;
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum FolderTreeHoverScroll {
    Up,
    #[default]
    None,
    Down,
}

impl FolderTreeHoverScroll {
    pub(super) fn sign(self) -> f32 {
        match self {
            Self::Up => 1.0,
            Self::None => 0.0,
            Self::Down => -1.0,
        }
    }
}

/// menu edge hover の最大速度。通常の wheel/page scroll を置き換えず、狭い edge zone
/// に留めたときだけ読み切れる速さで進める。
pub(super) const FOLDER_TREE_HOVER_SCROLL_PX_PER_SECOND: f32 = 360.0;
pub(super) const FOLDER_TREE_HOVER_SCROLL_MAX_FRAME_SECONDS: f32 = 1.0 / 20.0;
/// v5 の font-relative 幅より少し余裕を持たせる。長い日本語 path でも一文字
/// ずつの細い menu にならず、狭い viewport では下の resolved width で縮む。
pub(super) const FOLDER_TREE_PREFERRED_COLUMN_WIDTH: f32 = 288.0;
/// cascade が「展開した」と認識できる最低幅。これより細い列を出すと、行本体と
/// 36px arrow のどちらも実用にならない。
pub(super) const FOLDER_TREE_MIN_COLUMN_WIDTH: f32 = 160.0;
pub(super) const FOLDER_TREE_MAX_HEIGHT: f32 = 440.0;
pub(super) const FOLDER_TREE_EDGE_MARGIN: f32 = 4.0;
const FOLDER_TREE_ROW_ESTIMATED_HEIGHT: f32 = 36.0;
const FOLDER_TREE_ROW_OVERSCAN: usize = 2;

#[derive(Clone, Copy, Debug)]
struct RowMeasurement {
    index: usize,
    measured_height: f32,
    trailing_gap: f32,
    scroll_offset: f32,
}

pub(super) fn folder_tree_column_layout_signature(
    column: &folder_tree_menu::Column,
) -> (u64, u8, usize, usize) {
    match &column.state {
        folder_tree_menu::ColumnState::Loading => (column.id, 0, 0, 0),
        folder_tree_menu::ColumnState::Ready(entries) => (column.id, 1, entries.len(), 0),
        folder_tree_menu::ColumnState::Error(message) => (column.id, 2, 1, message.chars().count()),
    }
}

fn folder_tree_unchanged_column_prefix(
    previous: &[(u64, u8, usize, usize)],
    current: &[(u64, u8, usize, usize)],
) -> usize {
    previous
        .iter()
        .zip(current)
        .take_while(|(previous, current)| previous == current)
        .count()
}

/// `Pixels` の `-0` を scroll 下端との比較へ渡さない。overflow が正でない列には、
/// edge scroll affordance 自体が存在しない。
pub(super) fn folder_tree_vertical_scroll_affordances(
    offset: Pixels,
    max_offset: Pixels,
) -> (bool, bool) {
    if max_offset <= px(0.0) {
        return (false, false);
    }
    (offset < px(0.0), offset > -max_offset)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FolderTreeChildPlacement {
    pub(super) width: Pixels,
    pub(super) opens_right: bool,
}

/// 親列を覆わない child column の幅と側を決める。
///
/// v5 と同じく通常幅を優先し、片側しか足りない時は反転する。両側が通常幅に
/// 足りなくても、最低幅を満たす広い側があれば child **だけ**を縮める。これで
/// 狭い window でも touch の expand が無反応に見えず、親行も隠さない。
pub(super) fn folder_tree_child_placement(
    parent_column: Bounds<Pixels>,
    viewport: gpui::Size<Pixels>,
) -> Option<FolderTreeChildPlacement> {
    // hover された row の実測幅は menu の padding を含まない。その row だけを
    // 基準にすると、空白 padding の上へ child が入り込んで親列を覆ってしまう。
    // 横方向の可用域は必ず親**列**の外側から測る。
    let parent_left = parent_column.origin.x.clamp(px(0.0), viewport.width);
    let parent_right =
        (parent_column.origin.x + parent_column.size.width).clamp(px(0.0), viewport.width);
    let left_available = parent_left;
    let right_available = viewport.width - parent_right;
    let preferred = px(FOLDER_TREE_PREFERRED_COLUMN_WIDTH);
    let minimum = px(FOLDER_TREE_MIN_COLUMN_WIDTH);

    if right_available >= preferred {
        return Some(FolderTreeChildPlacement {
            width: preferred,
            opens_right: true,
        });
    }
    if left_available >= preferred {
        return Some(FolderTreeChildPlacement {
            width: preferred,
            opens_right: false,
        });
    }

    let (width, opens_right) = if right_available >= left_available {
        (right_available, true)
    } else {
        (left_available, false)
    };
    (width >= minimum).then_some(FolderTreeChildPlacement { width, opens_right })
}

/// v5 の `bounds_element` と同じ bridge。Element を消去せず、その layout / prepaint
/// state をそのまま forward してから、paint で実 bounds だけを記録する。
pub(super) struct FolderTreeBounds<E> {
    pub(super) child: E,
    pub(super) bounds: Rc<RefCell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(super) on_paint: Option<Box<dyn FnMut(gpui::Bounds<gpui::Pixels>, bool, &mut Window)>>,
}

impl<E> gpui::IntoElement for FolderTreeBounds<E>
where
    E: gpui::Element,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E> gpui::Element for FolderTreeBounds<E>
where
    E: gpui::Element,
{
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
        self.child.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.child.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&gpui::GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        self.child.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&gpui::GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child
            .prepaint(id, inspector_id, bounds, state, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&gpui::GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let was_unpainted = self.bounds.borrow().is_none();
        *self.bounds.borrow_mut() = Some(bounds);
        if let Some(on_paint) = &mut self.on_paint {
            on_paint(bounds, was_unpainted, window);
        }
        self.child
            .paint(id, inspector_id, bounds, state, prepaint, window, cx);
    }
}

fn folder_tree_visible_range(heights: &RowHeights, offset: f32, viewport: f32) -> Range<usize> {
    heights.visible_range(offset, viewport, FOLDER_TREE_ROW_OVERSCAN)
}

fn folder_tree_refine_height(heights: &mut RowHeights, measurement: RowMeasurement) -> (bool, f32) {
    let Some(old) = heights.row_height(measurement.index) else {
        return (false, measurement.scroll_offset);
    };
    let delta = measurement.measured_height + measurement.trailing_gap - old.total();
    if delta.abs() <= f32::EPSILON {
        return (false, measurement.scroll_offset);
    }
    let anchor_index = heights.row_at_offset(measurement.scroll_offset.max(0.0));
    heights.set(
        measurement.index,
        RowHeight {
            content: measurement.measured_height,
            trailing: measurement.trailing_gap,
        },
    );
    let adjusted_scroll = if measurement.index < anchor_index {
        (measurement.scroll_offset + delta).max(0.0)
    } else if measurement.index == anchor_index {
        let within_row = measurement.scroll_offset - heights.offset_before(measurement.index);
        if within_row >= measurement.measured_height {
            heights.offset_before(measurement.index) + (measurement.measured_height - 1.0).max(0.0)
        } else {
            measurement.scroll_offset
        }
    } else {
        measurement.scroll_offset
    };
    (true, adjusted_scroll)
}

fn folder_tree_reset_estimates(heights: &mut RowHeights, len: usize) {
    heights.set_content(0..len.min(heights.len()), FOLDER_TREE_ROW_ESTIMATED_HEIGHT);
}

fn folder_tree_reset_estimates_preserving_anchor(
    heights: &mut RowHeights,
    len: usize,
    anchor_index: usize,
    within_row: f32,
) -> f32 {
    folder_tree_reset_estimates(heights, len);
    if anchor_index >= len {
        return heights.total_height();
    }
    let trailing = heights
        .row_height(anchor_index)
        .map_or(0.0, |row| row.trailing);
    let provisional_content =
        FOLDER_TREE_ROW_ESTIMATED_HEIGHT.max((within_row + 1.0 - trailing).max(0.0));
    heights.set_content(anchor_index..anchor_index + 1, provisional_content);
    heights.offset_before(anchor_index) + within_row
}

fn should_schedule_folder_tree_repaint(
    pending: &std::cell::Cell<bool>,
    measurement_changed: bool,
    restored_active_anchor: bool,
) -> bool {
    (measurement_changed || restored_active_anchor) && !pending.replace(true)
}

fn folder_tree_anchor_is_rendered(index: Option<usize>, range: &Range<usize>) -> bool {
    index.is_some_and(|index| range.contains(&index))
}

/// 開いている cascade は modal overlay であり、背景一覧へ scroll を渡してはならない。
/// child の既定 scroll を先に実行させてから bubble を止めるため、menu 列の慣性・wheel
/// は保ったまま、末端と Direct Manipulation の位置ずれでも背景を動かさない。
pub(super) struct FolderTreeScrollBarrier {
    pub(super) child: gpui::AnyElement,
}

/// Tree menu は shell icon service を起こさない短命の projection なので、埋込み Fluent
/// catalog だけで安定して判別できる種類を選ぶ。未知のものだけが通常 File / Folder になる。
pub(super) fn folder_tree_entry_icon(entry: &folder_tree_menu::Entry) -> IconId {
    if !entry.is_folder {
        return IconId::for_extension(
            std::path::Path::new(&entry.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or(""),
        );
    }

    let name = entry.name.as_str();
    if name.eq_ignore_ascii_case("desktop") {
        IconId::FolderDesktop
    } else if name.eq_ignore_ascii_case("documents") {
        IconId::FolderDocuments
    } else if name.eq_ignore_ascii_case("downloads") {
        IconId::FolderDownloads
    } else if name.eq_ignore_ascii_case("music") {
        IconId::FolderMusic
    } else if name.eq_ignore_ascii_case("pictures") {
        IconId::FolderPictures
    } else if name.eq_ignore_ascii_case("videos") {
        IconId::FolderVideos
    } else {
        IconId::Folder
    }
}

impl gpui::IntoElement for FolderTreeScrollBarrier {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for FolderTreeScrollBarrier {
    type RequestLayoutState = gpui::LayoutId;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let layout_id = self.child.request_layout(window, cx);
        (layout_id, layout_id)
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _layout_id: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _layout_id: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // wrapper は overlay child より先に登録する。bubble は後ろから前へ呼ばれるため、
        // menu 列の overflow scroll が先、ここが次、背後の directory list は最後となる。
        window.on_mouse_event(move |_: &gpui::ScrollWheelEvent, phase, _window, cx| {
            if phase == gpui::DispatchPhase::Bubble {
                cx.stop_propagation();
            }
        });
        self.child.paint(window, cx);
    }
}

impl FilerShell {
    pub(super) fn render_folder_tree_menu_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let menu = self.folder_tree_menu.as_mut()?;
        // Ready entry slices are immutable Arcs, so this snapshot copies only the
        // bounded column descriptors instead of the full directory projection.
        let columns = menu.model.columns().to_vec();
        let viewport = window.viewport_size();
        let layout_signature = columns
            .iter()
            .map(folder_tree_column_layout_signature)
            .collect::<Vec<_>>();
        let old_layout_signature = menu.column_layout_signature.clone();
        let layout_changed = menu.column_layout_signature != layout_signature;
        if layout_changed {
            // 変化した列とその子孫だけ bounds を無効化する。全列を消すと、
            // unchanged な親まで一度描画 gate に掛かり、cascade が段ごとに欠ける。
            menu.column_layout_signature = layout_signature.clone();
            let unchanged_prefix =
                folder_tree_unchanged_column_prefix(&old_layout_signature, &layout_signature);
            menu.column_bounds.truncate(unchanged_prefix);
            menu.column_bounds
                .extend((unchanged_prefix..columns.len()).map(|_| Rc::new(RefCell::new(None))));
            menu.entry_anchors.truncate(unchanged_prefix);
            menu.entry_anchor_indices.truncate(unchanged_prefix);
        }
        let viewport_changed = menu.bounds_viewport != Some(viewport);
        if viewport_changed {
            // resize 後に前 viewport の anchor/bounds を使うと、child が古い側へ
            // 描かれて親列へ重なる。一旦描かず、prepaint 後の次 frame で復帰する。
            menu.bounds_viewport = Some(viewport);
            for anchor in &menu.entry_anchors {
                *anchor.borrow_mut() = None;
            }
            menu.column_bounds = (0..columns.len())
                .map(|_| Rc::new(RefCell::new(None)))
                .collect();
        }
        let column_ids = columns.iter().map(|column| column.id).collect::<Vec<_>>();
        if menu.column_scroll_ids != column_ids {
            // index N の child は hover 先が変われば別の column ID になる。古い handle を
            // そのまま使うと、以前の長い列の max offset から empty column にも edge
            // scroll affordance が出てしまう。同じ ID だけを残して状態を引き継ぐ。
            let old_ids = std::mem::take(&mut menu.column_scroll_ids);
            let old_scrolls = std::mem::take(&mut menu.column_scrolls);
            let old_hover = std::mem::take(&mut menu.hover_scroll);
            let mut scrolls = Vec::with_capacity(column_ids.len());
            let mut hover = Vec::with_capacity(column_ids.len());
            for column_id in &column_ids {
                if let Some(old_index) = old_ids.iter().position(|old_id| old_id == column_id) {
                    scrolls.push(old_scrolls[old_index].clone());
                    hover.push(old_hover.get(old_index).copied().unwrap_or_default());
                } else {
                    scrolls.push(ScrollHandle::new());
                    hover.push(FolderTreeHoverScroll::None);
                }
            }
            menu.column_scroll_ids = column_ids.clone();
            menu.column_scrolls = scrolls;
            menu.hover_scroll = hover;
        }
        if menu.column_height_ids != column_ids {
            let old_ids = std::mem::take(&mut menu.column_height_ids);
            let old_heights = std::mem::take(&mut menu.column_heights);
            let mut heights = Vec::with_capacity(column_ids.len());
            for column_id in &column_ids {
                if let Some(old_index) = old_ids.iter().position(|old_id| old_id == column_id) {
                    heights.push(old_heights[old_index].clone());
                } else {
                    heights.push(Rc::new(RefCell::new(RowHeights::new(0))));
                }
            }
            menu.column_height_ids = column_ids.clone();
            menu.column_heights = heights;
        }
        if layout_changed || viewport_changed {
            for (index, column) in columns.iter().enumerate() {
                let column_signature_changed =
                    old_layout_signature.get(index) != layout_signature.get(index);
                if !viewport_changed && !column_signature_changed {
                    continue;
                }
                let Some(row_heights) = menu.column_heights.get(index) else {
                    continue;
                };
                let len = match &column.state {
                    folder_tree_menu::ColumnState::Ready(entries) => entries.len(),
                    _ => 0,
                };
                let scroll = &menu.column_scrolls[index];
                let old_offset = -scroll.offset().y.as_f32();
                let (anchor, within_row) = {
                    let heights = row_heights.borrow();
                    let anchor = heights.row_at_offset(old_offset.max(0.0)).min(len);
                    let within = (old_offset - heights.offset_before(anchor)).max(0.0);
                    (anchor, within)
                };
                if column_signature_changed {
                    // ID/state/count が変わった列だけ index を作り直す。
                    let mut heights = RowHeights::new(len);
                    folder_tree_reset_estimates(&mut heights, len);
                    *row_heights.borrow_mut() = heights;
                    scroll.set_offset(gpui::point(px(0.0), px(0.0)));
                } else if viewport_changed {
                    // 幅だけ変わった場合は index を再確保しない。segment tree の
                    // range update で測定済み content だけを estimate へ戻し、gap と
                    // 現在の行内 offset は保つ。
                    let mut heights = row_heights.borrow_mut();
                    let restored_offset = folder_tree_reset_estimates_preserving_anchor(
                        &mut heights,
                        len,
                        anchor,
                        within_row,
                    );
                    scroll.set_offset(gpui::point(px(0.0), -px(restored_offset)));
                }
            }
        }
        // edge zone に居る間は秒あたりの速度で送る。frame ごとの固定 18px だと
        // 144Hz display で 2.5倍になり、frame stall 後も menu が読めないほど進む。
        // ScrollHandle は次の prepaint で clamp されるが、ここでも max を使い端で止める。
        let now = Instant::now();
        let elapsed = menu
            .hover_scroll_last
            .replace(now)
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(0.0)
            .min(FOLDER_TREE_HOVER_SCROLL_MAX_FRAME_SECONDS);
        let mut keep_hover_scrolling = false;
        for (scroll, direction) in menu.column_scrolls.iter().zip(menu.hover_scroll.iter()) {
            let sign = direction.sign();
            if sign == 0.0 {
                continue;
            }
            let current = scroll.offset().y.as_f32();
            let max = scroll.max_offset().y.as_f32();
            // hover 開始直後は elapsed=0 でも、次の animation frame を必ず予約する。
            // 端では予約しないため、見えない edge zone で空回りもしない。
            let can_continue = max > 0.0
                && if sign > 0.0 {
                    current < 0.0
                } else {
                    current > -max
                };
            keep_hover_scrolling |= can_continue;
            let delta = sign * FOLDER_TREE_HOVER_SCROLL_PX_PER_SECOND * elapsed;
            if delta == 0.0 {
                continue;
            }
            let next = (current + delta).clamp(-max, 0.0);
            if (next - current).abs() > f32::EPSILON {
                scroll.set_offset(gpui::point(px(0.0), px(next)));
                keep_hover_scrolling = true;
            }
        }
        if keep_hover_scrolling {
            window.request_animation_frame();
            cx.notify();
        }
        let anchor = menu.anchor;
        let anchor_kind = menu.kind;
        let column_scrolls = menu.column_scrolls.clone();
        let entry_anchors = menu.entry_anchors.clone();
        let entry_anchor_indices = menu.entry_anchor_indices.clone();
        let column_heights = menu.column_heights.clone();
        let measurement_notification_pending = menu.measurement_notification_pending.clone();
        let column_bounds = menu.column_bounds.clone();
        let needs_reflow_frame = column_bounds.iter().any(|bounds| bounds.borrow().is_none());
        // 開く操作は mouse-down で届く。ここも mouse-down にすると同じ入力で
        // 即座に閉じてしまうため、背景の通常 click でだけ閉じる。
        let backdrop = div()
            .id("folder-tree-menu-backdrop")
            .absolute()
            .size_full()
            .on_click(cx.listener(|this, _event: &gpui::ClickEvent, _window, cx| {
                this.close_folder_tree_menu(cx);
            }));
        let mut overlay = div()
            .id("folder-tree-menu-overlay")
            .absolute()
            .size_full()
            .child(backdrop);
        if needs_reflow_frame {
            // wrapping 後の実高は prepaint でしか得られない。二 frame 目で
            // placement を再評価し、短い menu を最大高として clamp しない。
            let shell = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = shell.update(cx, |_this, cx| cx.notify());
            });
        }
        // item は button の右上と menu 左上、breadcrumb は chevron の左下と
        // menu 左上を接続する。優先側が足りない時だけ反転し、狭い window では
        // 最後に画面内へ clamp する。
        let root_column_width = px(FOLDER_TREE_PREFERRED_COLUMN_WIDTH)
            .min((viewport.width - px(FOLDER_TREE_EDGE_MARGIN * 2.0)).max(px(0.0)));
        let menu_height = px(FOLDER_TREE_MAX_HEIGHT);
        let root_max_x = (viewport.width - root_column_width).max(px(0.0));
        let column_height = |index: usize| {
            column_bounds
                .get(index)
                .and_then(|bounds| *bounds.borrow())
                .map_or(menu_height, |bounds| bounds.size.height.min(menu_height))
        };
        let max_y_for_height = |height| (viewport.height - height).max(px(0.0));
        let root_max_y = max_y_for_height(column_height(0));
        let (root_x, root_y, _opens_right) = match anchor_kind {
            FolderTreeAnchorKind::Item => {
                let right_x = anchor.origin.x + anchor.size.width;
                let left_x = anchor.origin.x - root_column_width;
                let opens_right = right_x + root_column_width <= viewport.width || left_x < px(0.0);
                let x = if opens_right { right_x } else { left_x };
                (
                    x.clamp(px(0.0), root_max_x),
                    anchor.origin.y.clamp(px(0.0), root_max_y),
                    opens_right,
                )
            }
            FolderTreeAnchorKind::Breadcrumb => {
                let right_x = anchor.origin.x;
                let left_x = anchor.origin.x + anchor.size.width - root_column_width;
                let opens_right = right_x + root_column_width <= viewport.width || left_x < px(0.0);
                let x = if opens_right { right_x } else { left_x };
                let y = (anchor.origin.y + anchor.size.height).clamp(px(0.0), root_max_y);
                (x.clamp(px(0.0), root_max_x), y, opens_right)
            }
        };
        for (column_index, column) in columns.into_iter().enumerate() {
            let column_scroll = column_scrolls[column_index].clone();
            let row_heights = column_heights[column_index].clone();
            let scroll_offset = -column_scroll.offset().y.as_f32();
            let measured_viewport = column_scroll.bounds().size.height.as_f32();
            let visible_viewport = if measured_viewport > 0.0 {
                measured_viewport.min(FOLDER_TREE_MAX_HEIGHT)
            } else {
                FOLDER_TREE_MAX_HEIGHT
            };
            let visible_range =
                folder_tree_visible_range(&row_heights.borrow(), scroll_offset, visible_viewport);
            if !folder_tree_anchor_is_rendered(
                entry_anchor_indices.get(column_index).copied().flatten(),
                &visible_range,
            ) {
                if let Some(anchor) = entry_anchors.get(column_index) {
                    *anchor.borrow_mut() = None;
                }
            }
            let parent_anchor = (column_index > 0)
                .then(|| {
                    entry_anchors
                        .get(column_index - 1)
                        .and_then(|anchor| *anchor.borrow())
                })
                .flatten();
            // 親 row が column の clip 範囲から出た child は描かない。scroll 後に
            // 画面端へ clamp された子列だけが浮くより、親が再び見えるまで閉じて
            // 見せる方が cascade の接続関係を保てる。
            let parent_column = column_index
                .checked_sub(1)
                .and_then(|index| column_bounds.get(index))
                .and_then(|bounds| *bounds.borrow());
            if let (Some(parent), Some(parent_column)) = (parent_anchor, parent_column)
                && (parent.origin.y + parent.size.height <= parent_column.origin.y
                    || parent.origin.y >= parent_column.origin.y + parent_column.size.height)
            {
                break;
            }
            let child_placement = match (parent_anchor, parent_column) {
                (Some(parent), Some(parent_column)) => {
                    let Some(placement) = folder_tree_child_placement(parent_column, viewport)
                    else {
                        // 両側とも最低幅未満。親列を覆う fallback は作らない。
                        // この極端な viewport では parent の action だけを残す。
                        break;
                    };
                    Some((parent, placement))
                }
                // 親列の実測前は安全な横配置を決められない。次 frame の再描画で
                // bounds を受け取ってから開く。
                (Some(_), None) => break,
                (None, _) if column_index > 0 => break,
                (None, _) => None,
            };
            let column_width = child_placement
                .map(|(_, placement)| placement.width)
                .unwrap_or(root_column_width);
            let column_layout_bounds = column_bounds[column_index].clone();
            let page_scroll = column_scroll.clone();
            let page_column = column_index;
            let mut body = div()
                .id(("folder-tree-menu-column", column.id))
                .w(column_width)
                // block flow では折返した child の実高を次 row の開始位置へ
                // 反映できず、長い folder 名の末尾が次項目と重なった。v5 と同じ
                // 明示的な縦 flex にして、各 row を content-driven height で積む。
                .flex()
                .flex_col()
                .max_h(menu_height)
                .overflow_y_scroll()
                .track_scroll(&column_scroll)
                // v5 の renderer と同じく、scrollable surface 自身を block hitbox にする。
                // 親の auto-size wrapper ではなくここに置くことで touch scroll の hit-test
                // でも背後の directory list が候補へ残らない。
                .occlude()
                .on_scroll_wheel(cx.listener(
                    move |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                        // Windows の WHEEL_PAGESCROLL sentinel は GPUI では line delta のまま
                        // 届く。directory list と同じ adapter で一画面分だけ送る。
                        if let gpui::ScrollDelta::Lines(delta) = event.delta
                            && delta.y.abs() >= 1_000_000.0
                        {
                            let scroll = &page_scroll;
                            let viewport = scroll.bounds().size.height.as_f32();
                            let offset = -scroll.offset().y.as_f32()
                                - delta.y.signum() * (viewport - 32.0).max(32.0);
                            scroll.set_offset(gpui::point(
                                px(0.0),
                                -px(offset.clamp(0.0, scroll.max_offset().y.as_f32())),
                            ));
                            // page scroll が端へ着いたら、消えた edge zone の hover state も
                            // 同じ frame で消す。残すと表示されない方向へ継続scrollしてしまう。
                            if offset <= 0.0 || offset >= scroll.max_offset().y.as_f32() {
                                this.set_folder_tree_hover_scroll(
                                    page_column,
                                    FolderTreeHoverScroll::None,
                                    cx,
                                );
                            }
                            cx.notify();
                        }
                        // overflow scroll が処理した wheel を directory list へ流さない。
                        cx.stop_propagation();
                    },
                ))
                .flex()
                .flex_col()
                .p(px(4.0))
                .gap(px(2.0))
                .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                .text_color(gpui_filer_ui::theme_palette(cx).text)
                .text_size(px(12.0))
                .border_1()
                .border_color(gpui_filer_ui::theme_palette(cx).border)
                .rounded(px(RADIUS_SM));
            match column.state {
                folder_tree_menu::ColumnState::Loading => {
                    body = body.child(
                        div()
                            .px(px(10.0))
                            .py(px(8.0))
                            .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                            .child("Loading…"),
                    );
                }
                folder_tree_menu::ColumnState::Error(error) => {
                    body = body.child(
                        div()
                            .min_w_0()
                            .px(px(10.0))
                            .py(px(8.0))
                            .whitespace_normal()
                            .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                            .child(error),
                    );
                }
                folder_tree_menu::ColumnState::Ready(entries) => {
                    let entry_count = entries.len();
                    if entries.is_empty() {
                        body = body.child(
                            div()
                                .px(px(10.0))
                                .py(px(8.0))
                                .text_color(gpui_filer_ui::theme_palette(cx).text_muted)
                                .child("(empty)"),
                        );
                    }
                    let heights = row_heights.borrow();
                    let leading_extent = heights.offset_before(visible_range.start);
                    let trailing_extent =
                        heights.total_height() - heights.offset_before(visible_range.end);
                    if visible_range.start > 0 {
                        body = body.child(
                            div()
                                .w_full()
                                .h(px((leading_extent - 2.0).max(0.0)))
                                .flex_shrink_0(),
                        );
                    }
                    for entry_index in visible_range.clone() {
                        let entry = entries[entry_index].clone();
                        let hover_entry = entry.clone();
                        let open_entry = entry.clone();
                        let entry_icon = folder_tree_entry_icon(&entry);
                        let build_row = || {
                            div()
                                .relative()
                                .w_full()
                                .min_h(px(36.0))
                                .px(px(8.0))
                                .py(px(5.0))
                                .flex()
                                // bounded scroll column の flex child を縮めると、項目数が
                                // 多いときに box だけが潰れて wrapped text が次 row へ重なる。
                                // row は content-driven height を保ち、列だけを scroll する。
                                .flex_shrink_0()
                                .items_center()
                                .text_color(gpui::rgb(0xffffff))
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_grow_1()
                                        .flex_shrink_1()
                                        .flex()
                                        // 行表示と同じく、icon は複数行 label の中央へ置く。
                                        .items_center()
                                        .gap(px(6.0))
                                        .pr(px(40.0))
                                        .child(div().w(px(16.0)).h(px(16.0)).flex_shrink_0().child(
                                            Icon::new(entry_icon.clone()).size(px(16.0)).color(
                                                gpui_filer_ui::theme_palette(cx).text_muted.into(),
                                            ),
                                        ))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .flex_grow_1()
                                                .flex_shrink_1()
                                                .whitespace_normal()
                                                .text_color(gpui::rgb(0xffffff))
                                                .child(entry.name.clone()),
                                        ),
                                )
                        };
                        let build_expand_indicator = || {
                            div()
                                .absolute()
                                .right_0()
                                .top_0()
                                .bottom_0()
                                .w(px(36.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(3.0))
                                .child(
                                    Icon::new(IconId::Expand)
                                        .size(px(10.0))
                                        .color(gpui_filer_ui::theme_palette(cx).text_muted.into()),
                                )
                        };
                        // 開いている child の親 row は、resize 後も同じ live cell を
                        // 描く。これにより無効化した anchor を次の prepaint で新しい
                        // 座標へ更新できる。
                        let bounds = menu
                            .model
                            .columns()
                            .get(column_index + 1)
                            .filter(|child| child.path == entry.path)
                            .and_then(|_| entry_anchors.get(column_index).cloned())
                            .unwrap_or_else(|| Rc::new(RefCell::new(None)));
                        let hover_bounds = bounds.clone();
                        let tap_bounds = bounds.clone();
                        let tap_entry = entry.clone();
                        let active_anchor = menu
                            .model
                            .columns()
                            .get(column_index + 1)
                            .is_some_and(|child| child.path == entry.path)
                            && entry_anchor_indices.get(column_index).copied().flatten()
                                == Some(entry_index);
                        let measurement_heights = row_heights.clone();
                        let measurement_scroll = column_scroll.clone();
                        let measurement_pending = measurement_notification_pending.clone();
                        let shell = cx.entity().downgrade();
                        let measurement = Box::new(
                            move |actual_bounds: gpui::Bounds<gpui::Pixels>,
                                  was_unpainted: bool,
                                  window: &mut Window| {
                                let old_scroll = -measurement_scroll.offset().y.as_f32();
                                let (changed, new_scroll) = folder_tree_refine_height(
                                    &mut measurement_heights.borrow_mut(),
                                    RowMeasurement {
                                        index: entry_index,
                                        measured_height: actual_bounds.size.height.as_f32(),
                                        trailing_gap: if entry_index + 1 < entry_count {
                                            2.0
                                        } else {
                                            0.0
                                        },
                                        scroll_offset: old_scroll,
                                    },
                                );
                                let restored_active_anchor = was_unpainted && active_anchor;
                                if !changed && !restored_active_anchor {
                                    return;
                                }
                                if (new_scroll - old_scroll).abs() > f32::EPSILON {
                                    measurement_scroll
                                        .set_offset(gpui::point(px(0.0), -px(new_scroll)));
                                }
                                if should_schedule_folder_tree_repaint(
                                    &measurement_pending,
                                    changed,
                                    restored_active_anchor,
                                ) {
                                    let pending = measurement_pending.clone();
                                    let shell = shell.clone();
                                    window.on_next_frame(move |_, cx| {
                                        let _ = shell.update(cx, |_this, cx| {
                                            pending.set(false);
                                            cx.notify();
                                        });
                                    });
                                }
                            },
                        );
                        body = body.child(FolderTreeBounds {
                            bounds,
                            on_paint: Some(measurement),
                            child: build_row()
                                .id(format!(
                                    "folder-tree-menu-entry-{}-{entry_index}",
                                    column.id
                                ))
                                .rounded(px(3.0))
                                .hover(|row| row.bg(gpui_filer_ui::theme_palette(cx).chrome_hover))
                                .when(entry.is_folder, |row| {
                                    row.child(
                                        build_expand_indicator()
                                            .id(format!(
                                                "folder-tree-menu-expand-{}-{entry_index}",
                                                column.id
                                            ))
                                            // hover 用の記号だけでなく、指でも確実に押せる幅。
                                            .hover(|arrow| arrow.bg(gpui_filer_ui::theme_palette(cx).chrome_hover_strong))
                                            .active(|arrow| arrow.bg(gpui_filer_ui::theme_palette(cx).accent_hover))
                                            .on_click(cx.listener(
                                                move |this, event: &gpui::ClickEvent, window, cx| {
                                                    cx.stop_propagation();
                                                    if event.standard_click()
                                                        && tap_bounds.borrow().is_some()
                                                    {
                                                        this.hover_folder_tree_entry(
                                                            column_index,
                                                            entry_index,
                                                            tap_entry.clone(),
                                                            tap_bounds.clone(),
                                                            cx,
                                                        );
                                                        // touch tap は hover transition を作らない。
                                                        // 展開後も list へ focus を渡さない。
                                                        window.refresh();
                                                    }
                                                },
                                            )),
                                    )
                                })
                                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                                    if *hovered && hover_bounds.borrow().is_some() {
                                        this.hover_folder_tree_entry(
                                            column_index,
                                            entry_index,
                                            hover_entry.clone(),
                                            hover_bounds.clone(),
                                            cx,
                                        );
                                    }
                                }))
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, window, cx| {
                                        cx.stop_propagation();
                                        if event.standard_click() {
                                            this.activate_folder_tree_entry(
                                                open_entry.clone(),
                                                window,
                                                cx,
                                            );
                                        }
                                    },
                                )),
                        });
                    }
                    drop(heights);
                    if visible_range.end < entry_count {
                        body = body.child(
                            div()
                                .w_full()
                                .h(px((trailing_extent - 2.0).max(0.0)))
                                .flex_shrink_0(),
                        );
                    }
                }
            }
            let (top_scrollable, bottom_scrollable) = folder_tree_vertical_scroll_affordances(
                column_scroll.offset().y,
                column_scroll.max_offset().y,
            );
            let mut column_surface = div()
                .relative()
                .w(column_width)
                .max_h(menu_height)
                .child(body);
            if top_scrollable {
                column_surface = column_surface.child(
                    div()
                        .id(("folder-tree-menu-scroll-up", column.id))
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                        .occlude()
                        .child(
                            Icon::new(IconId::ChevronUp)
                                .size(px(10.0))
                                .color(gpui_filer_ui::theme_palette(cx).text_muted.into()),
                        )
                        .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                            this.set_folder_tree_hover_scroll(
                                column_index,
                                if *hovered {
                                    FolderTreeHoverScroll::Up
                                } else {
                                    FolderTreeHoverScroll::None
                                },
                                cx,
                            );
                        })),
                );
            }
            if bottom_scrollable {
                column_surface = column_surface.child(
                    div()
                        .id(("folder-tree-menu-scroll-down", column.id))
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(gpui_filer_ui::theme_palette(cx).surface_raised)
                        .occlude()
                        .child(
                            Icon::new(IconId::ChevronDown)
                                .size(px(10.0))
                                .color(gpui_filer_ui::theme_palette(cx).text_muted.into()),
                        )
                        .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                            this.set_folder_tree_hover_scroll(
                                column_index,
                                if *hovered {
                                    FolderTreeHoverScroll::Down
                                } else {
                                    FolderTreeHoverScroll::None
                                },
                                cx,
                            );
                        })),
                );
            }
            let (left, top) = if column_index == 0 {
                (root_x, root_y)
            } else if let Some((parent, placement)) = child_placement {
                // 子列は親 column の端ではなく、hover された**項目**の右上／左上に接続する。
                // 右が足りなければ左へ反転し、両側が狭ければ広い側へ child
                // だけ縮める。親 column と重ねる fallback は持たない。
                let right_x = parent.origin.x + parent.size.width;
                let x = if placement.opens_right {
                    right_x
                } else {
                    parent.origin.x - placement.width
                };
                let y = parent
                    .origin
                    .y
                    .clamp(px(0.0), max_y_for_height(column_height(column_index)));
                (x, y)
            } else {
                // hover callback が届く前の loading frame。root からずらさず、誤った
                // cascade の位置を一瞬表示しない。
                (root_x, root_y)
            };
            overlay = overlay.child(
                div()
                    .absolute()
                    .left(left)
                    .top(top)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(FolderTreeBounds {
                        child: column_surface.into_any_element(),
                        bounds: column_layout_bounds,
                        on_paint: None,
                    }),
            );
        }
        Some(
            FolderTreeScrollBarrier {
                child: overlay.into_any_element(),
            }
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod virtualized_row_tests {
    use super::*;

    #[test]
    fn folder_tree_layout_changes_preserve_unchanged_ancestor_prefix() {
        let root = (1, 1, 100, 0);
        let child_loading = (2, 0, 0, 0);
        let child_ready = (2, 1, 40, 0);
        let replacement_root = (3, 1, 100, 0);

        assert_eq!(
            folder_tree_unchanged_column_prefix(&[root], &[root, child_loading]),
            1
        );
        assert_eq!(
            folder_tree_unchanged_column_prefix(&[root, child_loading], &[root, child_ready]),
            1
        );
        assert_eq!(
            folder_tree_unchanged_column_prefix(&[root, child_ready], &[replacement_root]),
            0
        );
        assert_eq!(
            folder_tree_unchanged_column_prefix(&[root, child_ready], &[root]),
            1
        );
    }

    fn estimated_rows(len: usize) -> RowHeights {
        let mut rows = RowHeights::new(len);
        rows.set_content(0..len, FOLDER_TREE_ROW_ESTIMATED_HEIGHT);
        rows
    }

    #[test]
    fn large_column_range_stays_viewport_bounded_and_tracks_measured_heights() {
        let mut rows = estimated_rows(100_000);
        let initial = folder_tree_visible_range(&rows, 18_000.0, FOLDER_TREE_MAX_HEIGHT);
        assert!(initial.len() <= 20);

        let (changed, corrected_scroll) = folder_tree_refine_height(
            &mut rows,
            RowMeasurement {
                index: 505,
                measured_height: 90.0,
                trailing_gap: 2.0,
                scroll_offset: 18_000.0,
            },
        );
        assert!(changed);
        assert_eq!(corrected_scroll, 18_000.0);
        assert_eq!(rows.row_height(505).unwrap().content, 90.0);
        assert_eq!(rows.row_height(505).unwrap().trailing, 2.0);
        let after = folder_tree_visible_range(&rows, corrected_scroll, FOLDER_TREE_MAX_HEIGHT);
        assert!(after.len() <= 20);
    }

    #[test]
    fn measuring_rows_above_viewport_preserves_scroll_anchor() {
        let mut rows = estimated_rows(1_000);
        let scroll = rows.offset_before(100) + 7.0;
        let anchor_screen_y = rows.offset_before(100) - scroll;

        let (changed, corrected_scroll) = folder_tree_refine_height(
            &mut rows,
            RowMeasurement {
                index: 3,
                measured_height: 74.0,
                trailing_gap: 2.0,
                scroll_offset: scroll,
            },
        );

        assert!(changed);
        assert_eq!(corrected_scroll, scroll + 40.0);
        assert_eq!(rows.offset_before(100) - corrected_scroll, anchor_screen_y);
    }

    #[test]
    fn measured_row_below_scroll_anchor_does_not_change_scroll_offset() {
        let mut rows = estimated_rows(1_000);
        let scroll = rows.offset_before(100) + 7.0;
        let (changed, corrected_scroll) = folder_tree_refine_height(
            &mut rows,
            RowMeasurement {
                index: 120,
                measured_height: 74.0,
                trailing_gap: 2.0,
                scroll_offset: scroll,
            },
        );

        assert!(changed);
        assert_eq!(corrected_scroll, scroll);
        assert_eq!(rows.row_height(120).unwrap().trailing, 2.0);
    }

    #[test]
    fn resize_estimate_keeps_tall_row_anchor_until_shorter_measurement_clamps_within_it() {
        let mut rows = estimated_rows(10_000);
        let anchor_index = 40;
        rows.set(
            anchor_index,
            RowHeight {
                content: 600.0,
                trailing: 2.0,
            },
        );
        let old_scroll = rows.offset_before(anchor_index) + 250.0;
        assert_eq!(rows.row_at_offset(old_scroll), anchor_index);
        let within_row = old_scroll - rows.offset_before(anchor_index);

        let restored_scroll = folder_tree_reset_estimates_preserving_anchor(
            &mut rows,
            10_000,
            anchor_index,
            within_row,
        );
        assert_eq!(rows.row_at_offset(restored_scroll), anchor_index);
        assert!(
            folder_tree_visible_range(&rows, restored_scroll, FOLDER_TREE_MAX_HEIGHT).len() <= 20
        );

        let (changed, corrected_scroll) = folder_tree_refine_height(
            &mut rows,
            RowMeasurement {
                index: anchor_index,
                measured_height: 80.0,
                trailing_gap: 2.0,
                scroll_offset: restored_scroll,
            },
        );

        assert!(changed);
        assert_eq!(rows.row_at_offset(corrected_scroll), anchor_index);
        assert!(corrected_scroll - rows.offset_before(anchor_index) < 80.0);
        assert_eq!(rows.row_height(anchor_index).unwrap().trailing, 2.0);
        assert!(
            folder_tree_visible_range(&rows, corrected_scroll, FOLDER_TREE_MAX_HEIGHT).len() <= 20
        );
    }

    #[test]
    fn resetting_large_column_estimates_reuses_index_and_preserves_row_gaps() {
        let mut rows = estimated_rows(100_000);
        rows.set(
            50_000,
            RowHeight {
                content: 90.0,
                trailing: 2.0,
            },
        );

        folder_tree_reset_estimates(&mut rows, 100_000);

        assert_eq!(
            rows.row_height(50_000).unwrap().content,
            FOLDER_TREE_ROW_ESTIMATED_HEIGHT
        );
        assert_eq!(rows.row_height(50_000).unwrap().trailing, 2.0);
    }

    #[test]
    fn anchor_restoration_schedules_one_coalesced_repaint() {
        let pending = std::cell::Cell::new(false);

        assert!(should_schedule_folder_tree_repaint(&pending, false, true));
        assert!(!should_schedule_folder_tree_repaint(&pending, false, true));
        assert!(!should_schedule_folder_tree_repaint(&pending, false, false));
        pending.set(false);
        assert!(should_schedule_folder_tree_repaint(&pending, true, false));
    }

    #[test]
    fn offscreen_parent_anchor_is_not_currently_painted() {
        assert!(!folder_tree_anchor_is_rendered(Some(4), &(8..16)));
        assert!(!folder_tree_anchor_is_rendered(None, &(0..10)));
        assert!(folder_tree_anchor_is_rendered(Some(9), &(8..16)));
    }
}
