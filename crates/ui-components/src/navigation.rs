use crate::Icon;
use crate::scrollbar::horizontal_scrollbar;
use crate::theme::RADIUS_SM;
use crate::theme_palette;
use gpui::{
    Animation, AnimationExt as _, App, AppContext as _, Bounds, Context, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, RenderOnce, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, prelude::FluentBuilder as _,
    px, size,
};
use gpui_filer_resources::IconId;
use std::collections::{HashMap, HashSet};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone)]
pub enum BreadcrumbIntent {
    /// パンくず右端の余白から、現在地をパス文字列として直接編集する。
    EditPath,
    Activate {
        index: usize,
        new_tab: bool,
    },
    /// v5 と同じ、separator から child cascade を開く要求。
    OpenFolderTree {
        index: usize,
        anchor: Bounds<Pixels>,
    },
}
type BreadcrumbListener = Rc<dyn Fn(&BreadcrumbIntent, &mut Window, &mut App)>;
type NavigationDropTargetDecorator = Rc<dyn Fn(usize, gpui::Div) -> gpui::Div>;

/// パンくず区切りの child cascade をタップで開けるようにする hit 領域の幅。
/// 矢印は区切りの中央に置く。
const SEPARATOR_HIT_WIDTH: f32 = 44.0;
/// パンくずの左右余白の単位。
///
/// 途中の階層はこの半分を持ち、残りは区切りのhit領域が受け持つ。
/// 全部をパンくず側へ寄せると、hoverの背景が文字へ密着して寸詰まりに見える。
const CRUMB_PADDING: f32 = 5.0;
/// バーの右端に確保する余白の幅。タッチの目安（44）と同じ。
///
/// **ここはまだ何も起きない。** アドレス編集（余白クリックでパス入力）の入口になる予定で、
/// 長い path でも押せる場所が残るように先に幅だけ確保している。TODO の Phase D を参照。
const BLANK_WIDTH: f32 = 44.0;
/// バーの高さ。パンくずの hit 領域（44）の上下に 4px ずつ空く。
const BAR_HEIGHT: f32 = 52.0;

/// パスの解析や移動は親が担当する。部品は表示順のindexと操作意図だけを返す。
///
/// 子要素は `crumb, separator, crumb, ...` の順に並ぶ。呼び出し側は `scroll_to_item` で
/// `crumb i` を `i * 2` として指すため、この並びを変えない。
///
/// `hovered` はバーにポインタが乗っているか。横スクロールバーのつまみを出し入れするためだけに
/// 使う（部品は状態を持てないので、解決済みの状態を受け取る）。
#[derive(IntoElement)]
pub struct Breadcrumbs {
    labels: Vec<SharedString>,
    scroll: ScrollHandle,
    hovered: bool,
    on_intent: BreadcrumbListener,
    drop_target_decorator: Option<NavigationDropTargetDecorator>,
}
impl Breadcrumbs {
    pub fn new(
        labels: Vec<SharedString>,
        scroll: ScrollHandle,
        hovered: bool,
        on_intent: impl Fn(&BreadcrumbIntent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            labels,
            scroll,
            hovered,
            on_intent: Rc::new(on_intent),
            drop_target_decorator: None,
        }
    }

    /// Application-owned drop presentation can decorate filesystem-backed crumbs.
    pub fn with_drop_target_decorator(
        mut self,
        decorator: impl Fn(usize, gpui::Div) -> gpui::Div + 'static,
    ) -> Self {
        self.drop_target_decorator = Some(Rc::new(decorator));
        self
    }
}
impl RenderOnce for Breadcrumbs {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let count = self.labels.len();
        let drop_target_decorator = self.drop_target_decorator;
        let mut row = div()
            .id("breadcrumbs")
            .size_full()
            .flex()
            .items_center()
            .px(px(8.0))
            .overflow_x_scroll()
            .track_scroll(&self.scroll)
            .text_size(px(12.0));
        for (index, label) in self.labels.into_iter().enumerate() {
            let click = self.on_intent.clone();
            let middle = self.on_intent.clone();
            // 現在のフォルダだけをチップとして見せる。途中の階層は素のテキストに近づける。
            // 全部に左右余白を入れると、区切りのhit領域と合わせて間延びして見える。
            let active = index + 1 == count;
            let mut crumb = div();
            if let Some(decorator) = drop_target_decorator.as_ref() {
                crumb = decorator(index, crumb);
            }
            row = row.child(
                crumb
                    .id(("crumb", index))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(44.0))
                    .min_w(px(44.0))
                    .max_w(px(220.0))
                    .rounded(px(RADIUS_SM))
                    .px(px(CRUMB_PADDING))
                    // チップは同じ値をpaddingとmarginへ入れる。背景が区切りへ接しない。
                    .when(active, |chip| {
                        chip.px(px(CRUMB_PADDING * 2.0))
                            .mx(px(CRUMB_PADDING))
                            .bg(theme_palette(cx).chrome_hover)
                    })
                    .hover(|style| style.bg(theme_palette(cx).chrome_hover_strong))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(label),
                    )
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        if event.standard_click() {
                            click(
                                &BreadcrumbIntent::Activate {
                                    index,
                                    new_tab: event.modifiers().control,
                                },
                                window,
                                cx,
                            );
                        }
                    })
                    .on_mouse_down(gpui::MouseButton::Middle, move |_, window, cx| {
                        cx.stop_propagation();
                        middle(
                            &BreadcrumbIntent::Activate {
                                index,
                                new_tab: true,
                            },
                            window,
                            cx,
                        );
                    }),
            );
            if index + 1 < count {
                // 区切りは幅をhit領域として持ち、矢印は中央に置く。
                let open_tree = self.on_intent.clone();
                let bounds = Rc::new(RefCell::new(None));
                let bounds_for_paint = bounds.clone();
                let bounds_for_click = bounds.clone();
                row = row.child(
                    div()
                        .id(("crumb-separator", index))
                        .relative()
                        .w(px(SEPARATOR_HIT_WIDTH))
                        .h(px(44.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(RADIUS_SM))
                        .hover(|separator| {
                            separator
                                .bg(theme_palette(cx).chrome_hover)
                                .text_color(theme_palette(cx).text)
                        })
                        .child(
                            canvas(
                                move |painted, _window, _cx| {
                                    *bounds_for_paint.borrow_mut() = Some(painted);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(
                            Icon::new(IconId::Expand)
                                .size(px(12.0))
                                .color(theme_palette(cx).text_icon.into()),
                        )
                        .on_click(move |event, window, cx| {
                            // separator は child tree だけを開く hit target。
                            // bar の「余白をクリックしてアドレスを編集する」listener
                            // へ bubbling させると、menu を開いた直後に input が
                            // mount されてしまう。
                            cx.stop_propagation();
                            if event.standard_click() {
                                let anchor = bounds_for_click.borrow().unwrap_or_else(|| {
                                    Bounds::new(
                                        event.position(),
                                        size(px(SEPARATOR_HIT_WIDTH), px(44.0)),
                                    )
                                });
                                open_tree(
                                    &BreadcrumbIntent::OpenFolderTree { index, anchor },
                                    window,
                                    cx,
                                );
                            }
                        }),
                );
            }
        }
        // 末尾の touch 用余白も contents に含める。bar の外側に固定すると横幅を
        // 不必要に奪うため、横スクロールの末尾まで一緒に動かす。
        row = row.child(
            div()
                .id("breadcrumb-blank")
                .w(px(BLANK_WIDTH))
                .h_full()
                .flex_shrink_0(),
        );
        let scrollbar = horizontal_scrollbar(
            "breadcrumb-scrollbar",
            &self.scroll,
            self.hovered,
            theme_palette(cx),
        );
        // item/separator 自身は click を stop するため、bar 全体の listener はそれ以外、
        // すなわち横長の空白だけを address editor の入口にする。
        let edit_path = self.on_intent.clone();
        div()
            .id("breadcrumb-bar")
            .h(px(BAR_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme_palette(cx).surface_bar)
            .cursor_pointer()
            .on_click(move |event, window, cx| {
                if event.standard_click() {
                    cx.stop_propagation();
                    edit_path(&BreadcrumbIntent::EditPath, window, cx);
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    // 横スクロールバーを重ねるため。
                    .relative()
                    .child(row)
                    .children(scrollbar),
            )
    }
}

#[derive(Clone)]
pub struct TabLabel {
    pub id: usize,
    pub title: SharedString,
    pub location: SharedString,
    /// Shell-resolved parent-folder hint for duplicate locked tab titles.
    pub disambiguation: Option<SharedString>,
    /// lock の authority は shell。component は表示と intent だけを担う。
    pub locked: bool,
}

/// Application-owned copy for the vertical tab component.
///
/// The component deliberately receives resolved labels rather than a locale or
/// message identifier so localization policy stays outside `gpui-filer-ui`.
pub struct VerticalTabsText {
    pub heading: SharedString,
    pub show_heading: bool,
    pub new_tab: SharedString,
}

/// Keeps the currently recognized row drag across Shell-driven tab-list renders.
#[derive(Clone, Default)]
pub struct TabReorderInteraction {
    placeholder_tab: Rc<Cell<Option<usize>>>,
    click_guard_tab: Rc<Cell<Option<usize>>>,
    preview: Rc<Cell<Option<TabPreview>>>,
    transition: Rc<RefCell<TabPreviewTransition>>,
    transition_source: Rc<Cell<Option<usize>>>,
    candidate: Rc<Cell<Option<TabDropCandidate>>>,
    list_origin_y: Rc<Cell<Option<Pixels>>>,
    last_order: Rc<RefCell<Vec<(usize, bool)>>>,
    committed_faces: Rc<RefCell<Vec<(usize, f32, f32)>>>,
    committed_order: Rc<RefCell<Vec<usize>>>,
    label_transitions: Rc<RefCell<HashMap<usize, TabLabelTransition>>>,
    last_labels:
        Rc<RefCell<HashMap<usize, (SharedString, SharedString, bool, Option<SharedString>)>>>,
    next_label_generation: Rc<Cell<u64>>,
    face_transitions: Rc<RefCell<HashMap<usize, TabFaceTransition>>>,
    last_active_tab: Rc<Cell<Option<usize>>>,
    next_face_generation: Rc<Cell<u64>>,
}

#[derive(Clone)]
struct TabLabelTransition {
    generation: u64,
    from: Vec<TabLabelLayer>,
    started: std::time::Instant,
}

#[derive(Clone)]
struct TabLabelLayer {
    label: TabLabel,
    opacity: f32,
    offset_x: f32,
}

#[derive(Clone, Copy)]
struct TabFaceTransition {
    generation: u64,
    from_inset: f32,
    to_inset: f32,
    started: std::time::Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabPreview {
    Gap(usize),
    Swap(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabDropCandidate {
    Gap {
        index: usize,
        before_id: Option<usize>,
    },
    Swap {
        target_id: usize,
        target_locked: bool,
    },
}

impl TabDropCandidate {
    fn preview(self) -> TabPreview {
        match self {
            Self::Gap { index, .. } => TabPreview::Gap(index),
            Self::Swap { target_id, .. } => TabPreview::Swap(target_id),
        }
    }
}

#[derive(Default)]
struct TabPreviewTransition {
    generation: u64,
    started: Option<std::time::Instant>,
    from_source_height: f32,
    source_height: f32,
    from_gap_heights: Vec<f32>,
    from_swap_offset: f32,
    to_swap_offset: f32,
    returning_swaps: Vec<(usize, f32, f32, std::time::Instant, u64)>,
}

impl TabReorderInteraction {
    fn reconcile_active_faces(&self, tabs: &[TabLabel], active: usize) {
        let now = std::time::Instant::now();
        let current_ids: HashSet<_> = tabs.iter().map(|tab| tab.id).collect();
        let mut transitions = self.face_transitions.borrow_mut();
        transitions.retain(|id, transition| {
            current_ids.contains(id)
                && now.duration_since(transition.started) < TAB_PREVIEW_DURATION
        });
        let Some(previous_active) = self.last_active_tab.replace(Some(active)) else {
            return;
        };
        if previous_active == active {
            return;
        }
        for tab in tabs {
            let from_inset = transitions.get(&tab.id).map_or_else(
                || {
                    if tab.id == previous_active {
                        0.0
                    } else {
                        TAB_FACE_INSET
                    }
                },
                |transition| sample_face_inset(*transition, now),
            );
            let to_inset = if tab.id == active {
                0.0
            } else {
                TAB_FACE_INSET
            };
            if (from_inset - to_inset).abs() < f32::EPSILON {
                transitions.remove(&tab.id);
            } else {
                let generation = self.next_face_generation.get().wrapping_add(1).max(1);
                self.next_face_generation.set(generation);
                transitions.insert(
                    tab.id,
                    TabFaceTransition {
                        generation,
                        from_inset,
                        to_inset,
                        started: now,
                    },
                );
            }
        }
    }

    fn face_transition(&self, id: usize) -> Option<TabFaceTransition> {
        self.face_transitions.borrow().get(&id).copied()
    }

    fn next_label_generation(&self) -> u64 {
        let generation = self.next_label_generation.get().wrapping_add(1).max(1);
        self.next_label_generation.set(generation);
        generation
    }

    fn reconcile_label_transitions(&self, tabs: &[TabLabel]) {
        let now = std::time::Instant::now();
        let mut previous = self.last_labels.borrow_mut();
        let mut transitions = self.label_transitions.borrow_mut();
        transitions
            .retain(|_, transition| now.duration_since(transition.started) < TAB_PREVIEW_DURATION);
        let current_ids: HashSet<_> = tabs.iter().map(|tab| tab.id).collect();
        previous.retain(|id, _| current_ids.contains(id));
        for tab in tabs {
            let signature = (
                tab.title.clone(),
                tab.location.clone(),
                tab.locked,
                tab.disambiguation.clone(),
            );
            if let Some((title, location, locked, disambiguation)) = previous.get(&tab.id).cloned()
            {
                if title != signature.0
                    || location != signature.1
                    || locked != signature.2
                    || disambiguation != signature.3
                {
                    let prior = TabLabel {
                        id: tab.id,
                        title,
                        location,
                        locked,
                        disambiguation,
                    };
                    // 連続して状態が変わった場合は、直前の目標ではなく今画面に見える各層から
                    // 次の退場を始め、ラベルが瞬間的に元の濃さへ戻るのを避ける。
                    let from = transitions
                        .get(&tab.id)
                        .map(|transition| transition.visible_layers(&prior, now))
                        .unwrap_or_else(|| {
                            vec![TabLabelLayer {
                                label: prior,
                                opacity: 1.0,
                                offset_x: 0.0,
                            }]
                        });
                    transitions.insert(
                        tab.id,
                        TabLabelTransition {
                            generation: self.next_label_generation(),
                            from,
                            started: now,
                        },
                    );
                }
            }
            previous.insert(tab.id, signature);
        }
    }

    fn label_transition(&self, id: usize) -> Option<TabLabelTransition> {
        self.label_transitions.borrow().get(&id).cloned()
    }

    fn mark_drag_started(&self, source_id: usize, source_locked: bool) {
        self.placeholder_tab.set(Some(source_id));
        self.click_guard_tab.set(Some(source_id));
        self.preview.set(None);
        self.candidate.set(None);
        self.list_origin_y.set(None);
        self.last_order.borrow_mut().clear();
        self.committed_faces.borrow_mut().clear();
        self.committed_order.borrow_mut().clear();
        self.transition_source.set(None);
        let mut transition = self.transition.borrow_mut();
        transition.generation = transition.generation.wrapping_add(1);
        transition.started = None;
        transition.source_height = tab_row_height(source_locked);
        transition.from_source_height = transition.source_height;
        transition.from_gap_heights.clear();
        transition.from_swap_offset = 0.0;
        transition.to_swap_offset = 0.0;
        transition.returning_swaps.clear();
    }

    fn clear_drag_started(&self) {
        self.clear_placeholder();
        self.click_guard_tab.set(None);
    }

    pub fn clear_placeholder(&self) -> bool {
        let source_id = self.placeholder_tab.get();
        let was_gap_preview = matches!(self.preview.get(), Some(TabPreview::Gap(_)));
        let changed_preview = self.clear_candidate();
        let had_placeholder = self.placeholder_tab.replace(None).is_some();
        if was_gap_preview {
            self.transition_source.set(source_id);
        }
        had_placeholder | changed_preview
    }

    /// Clears a preview without revealing the source while its placeholder is active.
    fn clear_candidate(&self) -> bool {
        let gap_count = self.transition.borrow().from_gap_heights.len();
        self.candidate.set(None);
        self.list_origin_y.set(None);
        let tabs = self.last_order.borrow().clone();
        let changed_preview = self.set_preview(None, gap_count, None, None, &tabs);
        changed_preview
    }

    fn is_placeholder(&self, tab_id: usize) -> bool {
        self.placeholder_tab.get() == Some(tab_id)
    }

    fn is_settling_source(&self, tab_id: usize) -> bool {
        if self.transition_source.get() != Some(tab_id) || self.preview.get().is_some() {
            return false;
        }
        let finished = self
            .transition
            .borrow()
            .started
            .is_none_or(|started| started.elapsed() >= TAB_PREVIEW_DURATION);
        if finished {
            self.transition_source.set(None);
            false
        } else {
            true
        }
    }

    fn preview(&self) -> Option<TabPreview> {
        self.preview.get()
    }

    fn is_returning_swap(&self, tab_id: usize) -> bool {
        let transition = self.transition.borrow();
        transition
            .returning_swaps
            .iter()
            .any(|(id, _, _, started, _)| *id == tab_id && started.elapsed() < TAB_PREVIEW_DURATION)
    }

    fn is_committed_face(&self, tab_id: usize) -> bool {
        !self.committed_faces.borrow().is_empty()
            && self
                .transition
                .borrow()
                .started
                .is_some_and(|started| started.elapsed() < TAB_PREVIEW_DURATION)
            && self
                .committed_faces
                .borrow()
                .iter()
                .any(|(id, _, _)| *id == tab_id)
    }

    /// Starts a disposable visual settle only after Shell has accepted a changed order.
    pub fn accepted_order_change(&self, old_order: &[usize], new_order: &[usize]) -> bool {
        if old_order == new_order {
            self.clear_placeholder();
            return false;
        }
        let tabs = self.last_order.borrow().clone();
        let stored_order: Vec<_> = tabs.iter().map(|(id, _)| *id).collect();
        if stored_order != old_order {
            self.clear_placeholder();
            return false;
        }
        let preview = self.preview.get();
        let now = std::time::Instant::now();
        let transition = self.transition.borrow();
        let progress = transition.started.map_or(1.0, |started| {
            (now.duration_since(started).as_secs_f32() / TAB_PREVIEW_DURATION.as_secs_f32())
                .clamp(0.0, 1.0)
        });
        let eased = 1.0 - (1.0 - progress).powi(3);
        let active_swap_y = lerp(
            transition.from_swap_offset,
            transition.to_swap_offset,
            eased,
        );
        let source_height = transition.source_height;
        let release_gap_height = match preview {
            Some(TabPreview::Gap(index)) => {
                let to_height = transition.source_height;
                let from_height = transition
                    .from_gap_heights
                    .get(index)
                    .copied()
                    .unwrap_or(to_height);
                Some(lerp(from_height, to_height, eased))
            }
            _ => None,
        };
        drop(transition);
        let source_y = self
            .placeholder_tab
            .get()
            .and_then(|source| tabs.iter().position(|(id, _)| *id == source))
            .map_or(0.0, |index| self.layout_row_y(index, &tabs, preview));
        let source_id = self.placeholder_tab.get();
        let swap_target = match preview {
            Some(TabPreview::Swap(target_id)) => Some(target_id),
            _ => None,
        };
        let source_release_y = match preview {
            Some(TabPreview::Swap(target_id)) => tabs
                .iter()
                .position(|(id, _)| *id == target_id)
                .map_or(source_y, |index| self.layout_row_y(index, &tabs, preview)),
            Some(TabPreview::Gap(index)) => {
                let row_top = self.layout_row_y(index, &tabs, preview);
                let gap_height = release_gap_height.unwrap_or(TAB_GAP_HEIGHT);
                let face_offset = ((gap_height - source_height) / 2.0).max(-source_height / 2.0);
                // Use the in-flight gap size so an early release continues from what is visible.
                row_top - gap_height + face_offset
            }
            None => source_y,
        };
        let mut faces = Vec::new();
        for id in old_order {
            let (Some(old_index), Some(new_index)) = (
                old_order.iter().position(|candidate| candidate == id),
                new_order.iter().position(|candidate| candidate == id),
            ) else {
                continue;
            };
            if old_index == new_index {
                continue;
            }
            let from_y = if Some(*id) == source_id {
                source_release_y
            } else if Some(*id) == swap_target {
                source_y + active_swap_y
            } else {
                self.layout_row_y(old_index, &tabs, preview)
            };
            faces.push((*id, from_y, stable_row_y(new_index, &tabs)));
        }
        self.clear_placeholder();
        let mut transition = self.transition.borrow_mut();
        transition.returning_swaps.clear();
        transition.generation = transition.generation.wrapping_add(1);
        transition.started = Some(now);
        *self.committed_faces.borrow_mut() = faces;
        *self.committed_order.borrow_mut() = new_order.to_vec();
        true
    }

    fn validate_committed_order(&self, current_order: &[usize]) {
        let mut committed_order = self.committed_order.borrow_mut();
        if !committed_order.is_empty() && committed_order.as_slice() != current_order {
            committed_order.clear();
            self.committed_faces.borrow_mut().clear();
        }
    }

    fn layout_row_y(
        &self,
        index: usize,
        tabs: &[(usize, bool)],
        preview: Option<TabPreview>,
    ) -> f32 {
        let transition = self.transition.borrow();
        let progress = transition.started.map_or(1.0, |started| {
            (started.elapsed().as_secs_f32() / TAB_PREVIEW_DURATION.as_secs_f32()).clamp(0.0, 1.0)
        });
        let eased = 1.0 - (1.0 - progress).powi(3);
        let mut y = stable_row_y(index, tabs);
        for gap_index in 0..=index {
            let target_gap = if preview == Some(TabPreview::Gap(gap_index)) {
                transition.source_height
            } else {
                TAB_GAP_HEIGHT
            };
            let from_gap = transition
                .from_gap_heights
                .get(gap_index)
                .copied()
                .unwrap_or(target_gap);
            y += lerp(from_gap, target_gap, eased);
            if gap_index < index
                && self.placeholder_tab.get() == tabs.get(gap_index).map(|(id, _)| *id)
            {
                let target_source = if matches!(preview, Some(TabPreview::Gap(_))) {
                    0.0
                } else {
                    transition.source_height
                };
                y += lerp(transition.from_source_height, target_source, eased)
                    - transition.source_height;
            }
        }
        y
    }

    fn target_row_y(
        &self,
        index: usize,
        tabs: &[(usize, bool)],
        preview: Option<TabPreview>,
    ) -> f32 {
        let mut y = stable_row_y(index, tabs);
        let source_height = self.transition.borrow().source_height;
        for gap_index in 0..=index {
            if preview == Some(TabPreview::Gap(gap_index)) {
                y += source_height;
            }
            if gap_index < index
                && self.placeholder_tab.get() == tabs.get(gap_index).map(|(id, _)| *id)
                && matches!(preview, Some(TabPreview::Gap(_)))
            {
                y -= source_height;
            }
        }
        y
    }

    fn update_candidate(
        &self,
        candidate: Option<TabDropCandidate>,
        list_origin_y: Pixels,
        gap_count: usize,
        tabs: &[(usize, bool)],
        window: &mut Window,
    ) {
        self.list_origin_y.set(Some(list_origin_y));
        *self.last_order.borrow_mut() = tabs.to_vec();
        if self.candidate.replace(candidate) == candidate {
            return;
        }
        let next = candidate.map(TabDropCandidate::preview);
        let next_swap_offset = match (next, self.placeholder_tab.get()) {
            (Some(TabPreview::Swap(target_id)), Some(source_id)) => {
                match (
                    tabs.iter().position(|(id, _)| *id == source_id),
                    tabs.iter().position(|(id, _)| *id == target_id),
                ) {
                    (Some(_), Some(target_index)) => Some(
                        self.layout_row_y(target_index, tabs, self.preview.get())
                            - self.layout_row_y(
                                tabs.iter()
                                    .position(|(id, _)| *id == source_id)
                                    .unwrap_or(0),
                                tabs,
                                self.preview.get(),
                            ),
                    ),
                    _ => None,
                }
            }
            _ => None,
        };
        self.set_preview(next, gap_count, Some(window), next_swap_offset, tabs);
    }

    fn set_preview(
        &self,
        next: Option<TabPreview>,
        gap_count: usize,
        window: Option<&mut Window>,
        next_swap_offset: Option<f32>,
        tabs: &[(usize, bool)],
    ) -> bool {
        if self.preview.get() == next {
            return false;
        }
        let now = std::time::Instant::now();
        let previous = self.preview.get();
        let progress = self.transition.borrow().started.map_or(1.0, |started| {
            (now.duration_since(started).as_secs_f32() / TAB_PREVIEW_DURATION.as_secs_f32())
                .clamp(0.0, 1.0)
        });
        let eased = 1.0 - (1.0 - progress).powi(3);
        let source_id = self.placeholder_tab.get();
        let source_y = source_id
            .and_then(|id| tabs.iter().position(|(tab_id, _)| *tab_id == id))
            .map_or(12.0, |index| self.layout_row_y(index, tabs, previous));
        let previous_offset = {
            let transition = self.transition.borrow();
            lerp(
                transition.from_swap_offset,
                transition.to_swap_offset,
                eased,
            )
        };
        let previous_swap_y =
            matches!(previous, Some(TabPreview::Swap(_))).then(|| source_y + previous_offset);
        let returning_swap = if let Some(TabPreview::Swap(previous_id)) = previous {
            let from_y = previous_swap_y.unwrap_or(source_y);
            let to_y = tabs
                .iter()
                .position(|(tab_id, _)| *tab_id == previous_id)
                .map_or(from_y, |index| self.target_row_y(index, tabs, next));
            (next != Some(TabPreview::Swap(previous_id))).then_some((previous_id, from_y, to_y))
        } else {
            None
        };
        let mut transition = self.transition.borrow_mut();
        if let Some(returning_swap) = returning_swap {
            transition
                .returning_swaps
                .retain(|(id, _, _, _, _)| *id != returning_swap.0);
            let generation = transition.generation.wrapping_add(1);
            transition.returning_swaps.push((
                returning_swap.0,
                returning_swap.1,
                returning_swap.2,
                now,
                generation,
            ));
        }
        let previous_source = if matches!(previous, Some(TabPreview::Gap(_))) {
            0.0
        } else {
            transition.source_height
        };
        transition.from_source_height = lerp(transition.from_source_height, previous_source, eased);
        transition
            .from_gap_heights
            .resize(gap_count, TAB_GAP_HEIGHT);
        let source_height = transition.source_height;
        for (index, height) in transition.from_gap_heights.iter_mut().enumerate() {
            let previous_target = if previous == Some(TabPreview::Gap(index)) {
                source_height
            } else {
                TAB_GAP_HEIGHT
            };
            *height = lerp(*height, previous_target, eased);
        }
        if let (Some(TabPreview::Swap(next_id)), Some(next_swap_offset)) = (next, next_swap_offset)
        {
            // Retargeting a face that is already returning reverses from its sampled position.
            let from_y = transition
                .returning_swaps
                .iter()
                .find(|(id, _, _, _, _)| *id == next_id)
                .map(|(_, from_y, to_y, started, _)| {
                    let progress = (now.duration_since(*started).as_secs_f32()
                        / TAB_PREVIEW_DURATION.as_secs_f32())
                    .clamp(0.0, 1.0);
                    lerp(*from_y, *to_y, 1.0 - (1.0 - progress).powi(3))
                });
            transition.from_swap_offset = from_y.map_or(next_swap_offset, |y| y - source_y);
            transition.to_swap_offset = 0.0;
            transition
                .returning_swaps
                .retain(|(id, _, _, _, _)| *id != next_id);
        } else {
            transition.from_swap_offset = 0.0;
            transition.to_swap_offset = 0.0;
        }
        transition.generation = transition.generation.wrapping_add(1);
        transition.started = Some(now);
        self.preview.set(next);
        drop(transition);
        if let Some(window) = window {
            window.refresh();
        }
        true
    }

    fn source_height_animation(&self) -> (u64, f32, f32) {
        let transition = self.transition.borrow();
        let target = if matches!(self.preview.get(), Some(TabPreview::Gap(_))) {
            0.0
        } else {
            transition.source_height
        };
        (transition.generation, transition.from_source_height, target)
    }

    fn gap_height_animation(&self, index: usize) -> (u64, f32, f32) {
        let transition = self.transition.borrow();
        let target = if self.preview.get() == Some(TabPreview::Gap(index)) {
            transition.source_height
        } else {
            TAB_GAP_HEIGHT
        };
        (
            transition.generation,
            transition
                .from_gap_heights
                .get(index)
                .copied()
                .unwrap_or(target),
            target,
        )
    }

    fn swap_face_animation(&self) -> (u64, f32, f32) {
        let transition = self.transition.borrow();
        (
            transition.generation,
            transition.from_swap_offset,
            transition.to_swap_offset,
        )
    }

    fn returning_swap_animations(&self) -> Vec<(u64, usize, f32, f32)> {
        let transition = self.transition.borrow();
        transition
            .returning_swaps
            .iter()
            .filter(|(_, _, _, started, _)| started.elapsed() < TAB_PREVIEW_DURATION)
            .map(|(id, from_y, to_y, _, generation)| (*generation, *id, *from_y, *to_y))
            .collect()
    }

    fn committed_face_animations(&self) -> (u64, Vec<(usize, f32, f32)>) {
        let transition = self.transition.borrow();
        let active = transition
            .started
            .is_some_and(|started| started.elapsed() < TAB_PREVIEW_DURATION);
        (
            transition.generation,
            if active {
                self.committed_faces.borrow().clone()
            } else {
                Vec::new()
            },
        )
    }

    fn take_drag_for(&self, source_id: usize) -> bool {
        if self.click_guard_tab.get() == Some(source_id) {
            self.click_guard_tab.set(None);
            self.clear_placeholder();
            true
        } else {
            false
        }
    }
}

impl TabLabelTransition {
    fn visible_layers(
        &self,
        current_target: &TabLabel,
        now: std::time::Instant,
    ) -> Vec<TabLabelLayer> {
        let duration = TAB_PREVIEW_DURATION.as_secs_f32();
        let progress = (now.duration_since(self.started).as_secs_f32() / duration).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        let mut layers: Vec<_> = self
            .from
            .iter()
            .filter_map(|layer| {
                let opacity = layer.opacity * (1.0 - eased);
                (opacity > 0.001).then(|| TabLabelLayer {
                    label: layer.label.clone(),
                    opacity,
                    offset_x: lerp(layer.offset_x, -5.0, eased),
                })
            })
            .collect();
        if eased > 0.001 {
            layers.push(TabLabelLayer {
                label: current_target.clone(),
                opacity: eased,
                offset_x: 5.0 * (1.0 - eased),
            });
        }
        layers
    }
}

const TAB_SLOT_HEIGHT: f32 = 44.0;
const TAB_REGULAR_ROW_HEIGHT: f32 = 56.0;
const TAB_FACE_INSET: f32 = 4.0;
const TAB_EDGE_DROP_HEIGHT: f32 = TAB_FACE_INSET;
const TAB_GAP_HEIGHT: f32 = 0.0;
const TAB_PREVIEW_DURATION: std::time::Duration = std::time::Duration::from_millis(180);
const VERTICAL_TABS_OUTER_PADDING: f32 = 8.0;
const VERTICAL_TABS_CHILD_GAP: f32 = 8.0;
const VERTICAL_TABS_HEADING_HEIGHT: f32 = 36.0;
const VERTICAL_TABS_ACTION_HEIGHT: f32 = 44.0;
const VERTICAL_TABS_VISIBLE_ROW_SLACK: f32 = 8.0;
const VERTICAL_TABS_CHROME_HEIGHT: f32 =
    VERTICAL_TABS_OUTER_PADDING * 2.0 + VERTICAL_TABS_CHILD_GAP + VERTICAL_TABS_ACTION_HEIGHT;

/// `VerticalTabs` は各rowの前にアニメーション用gapを置く。
/// ScrollHandleへ渡す子indexを、タブの表示順indexから求める。
pub fn vertical_tab_scroll_child_index(tab_index: usize) -> usize {
    tab_index.saturating_mul(2).saturating_add(1)
}

fn tab_row_height(locked: bool) -> f32 {
    if locked {
        TAB_SLOT_HEIGHT
    } else {
        TAB_REGULAR_ROW_HEIGHT
    }
}

fn tab_face_target_inset(active: bool) -> f32 {
    if active { 0.0 } else { TAB_FACE_INSET }
}

#[cfg(test)]
#[test]
fn locked_and_regular_tabs_keep_distinct_stable_row_heights() {
    assert_eq!(tab_row_height(true), 44.0);
    assert_eq!(tab_row_height(false), 56.0);
    assert_eq!(tab_face_target_inset(true), 0.0);
    assert_eq!(tab_face_target_inset(false), TAB_FACE_INSET);
}

fn stable_row_y(index: usize, tabs: &[(usize, bool)]) -> f32 {
    tabs.iter()
        .take(index)
        .map(|(_, locked)| tab_row_height(*locked))
        .sum()
}

fn sample_face_inset(transition: TabFaceTransition, now: std::time::Instant) -> f32 {
    let progress = (now.duration_since(transition.started).as_secs_f32()
        / TAB_PREVIEW_DURATION.as_secs_f32())
    .clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - progress).powi(3);
    lerp(transition.from_inset, transition.to_inset, eased)
}

fn lerp(from: f32, to: f32, progress: f32) -> f32 {
    from + (to - from) * progress
}

fn canonical_tab_candidate(
    content_y: Pixels,
    source_id: usize,
    source_locked: bool,
    tabs: &[(usize, bool)],
) -> Option<TabDropCandidate> {
    let locked_count = tabs.iter().take_while(|(_, locked)| *locked).count();
    let gap_candidate = |index: usize| {
        let in_partition = if source_locked {
            index <= locked_count
        } else {
            index >= locked_count
        };
        if !in_partition {
            return None;
        }
        let before_id = if source_locked && index == locked_count {
            None
        } else {
            tabs.get(index).map(|(id, _)| *id)
        };
        (before_id != Some(source_id)).then_some(TabDropCandidate::Gap { index, before_id })
    };

    let mut slot_top = px(0.0);
    for (index, (target_id, target_locked)) in tabs.iter().enumerate() {
        let target_height = tab_row_height(*target_locked);
        let within_slot = content_y - slot_top;
        if within_slot >= px(0.0) && within_slot < px(TAB_EDGE_DROP_HEIGHT) {
            return gap_candidate(index);
        }
        if within_slot >= px(target_height - TAB_EDGE_DROP_HEIGHT)
            && within_slot < px(target_height)
        {
            return gap_candidate(index + 1);
        }
        if within_slot >= px(TAB_EDGE_DROP_HEIGHT)
            && within_slot < px(target_height - TAB_EDGE_DROP_HEIGHT)
        {
            let candidate = (*target_id != source_id && *target_locked == source_locked).then_some(
                TabDropCandidate::Swap {
                    target_id: *target_id,
                    target_locked: *target_locked,
                },
            );
            return candidate;
        }
        slot_top += px(target_height);
    }
    None
}

fn tab_candidate_at_pointer(
    pointer_y: Pixels,
    list_origin_y: Pixels,
    scroll_offset_y: Pixels,
    source_id: usize,
    source_locked: bool,
    tabs: &[(usize, bool)],
) -> Option<TabDropCandidate> {
    // GPUI offsets scrolled content negatively, so subtracting it maps viewport Y back to
    // the unanimated list coordinate used by the stable row/gap slots.
    let content_y = pointer_y - list_origin_y - scroll_offset_y;
    canonical_tab_candidate(content_y, source_id, source_locked, tabs)
}

fn tab_projection_face(
    cx: &App,
    title: SharedString,
    location: SharedString,
    locked: bool,
    disambiguation: Option<SharedString>,
    active: bool,
    can_close: bool,
) -> impl IntoElement {
    let foreground = if active {
        theme_palette(cx).on_accent
    } else {
        theme_palette(cx).text
    };
    let inset = tab_face_target_inset(active);
    let mut face = div()
        .relative()
        .w_full()
        .h(px(tab_row_height(locked)))
        .flex()
        .items_center()
        .pl(px(10.0))
        .text_size(px(12.0))
        .text_color(foreground)
        .child(
            div()
                .absolute()
                .top(px(inset))
                .bottom(px(inset))
                .left(px(0.0))
                .right(px(0.0))
                .rounded(px(RADIUS_SM))
                .bg(if active {
                    theme_palette(cx).accent
                } else {
                    theme_palette(cx).surface_raised
                }),
        )
        .child(tab_label_body(
            &TabLabel {
                id: 0,
                title,
                location,
                locked,
                disambiguation,
            },
            theme_palette(cx),
        ))
        .child(
            div()
                .size(px(44.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(if locked { IconId::Unlock } else { IconId::Lock })
                        .size(px(13.0))
                        .color(foreground.into()),
                ),
        );
    if can_close {
        face = face.child(
            div()
                .size(px(44.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconId::Close)
                        .size(px(13.0))
                        .color(foreground.into()),
                ),
        );
    }
    face
}

fn tab_label_body(label: &TabLabel, palette: crate::ThemePalette) -> gpui::Div {
    let main = if label.locked {
        let mut row = div()
            .flex()
            .items_center()
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .min_w_0()
                    .flex_shrink_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label.title.clone()),
            );
        if let Some(hint) = &label.disambiguation {
            row = row.child(
                div()
                    .w(px(48.0))
                    .ml(px(6.0))
                    .flex_shrink_0()
                    .text_size(px(10.0))
                    .text_color(palette.text_muted)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(hint.clone()),
            );
        }
        div().overflow_hidden().text_ellipsis().child(row)
    } else {
        div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label.title.clone()),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(palette.text_muted)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label.location.clone()),
            )
    };
    div().flex_1().min_w_0().child(main)
}

fn animated_tab_label_body(
    label: &TabLabel,
    transition: Option<TabLabelTransition>,
    palette: crate::ThemePalette,
) -> impl IntoElement {
    let Some(transition) = transition else {
        return div()
            .flex_1()
            .min_w_0()
            .child(tab_label_body(label, palette))
            .into_any_element();
    };
    let generation = transition.generation;
    let id = label.id;
    let incoming = tab_label_body(label, palette).with_animation(
        format!("tab-label-in-{id}-{generation}"),
        Animation::new(TAB_PREVIEW_DURATION),
        |element, t| {
            let eased = 1.0 - (1.0 - t).powi(3);
            element.opacity(eased).left(px(lerp(5.0, 0.0, eased)))
        },
    );
    let mut body = div().relative().flex_1().min_w_0().child(incoming);
    for (layer_index, layer) in transition.from.into_iter().enumerate() {
        let start_opacity = layer.opacity;
        let start_x = layer.offset_x;
        let outgoing = tab_label_body(&layer.label, palette)
            .absolute()
            .left(px(start_x))
            .top(px(0.0))
            .right(px(0.0))
            .w_full()
            .with_animation(
                format!("tab-label-out-{id}-{generation}-{layer_index}"),
                Animation::new(TAB_PREVIEW_DURATION),
                move |element, t| {
                    let eased = 1.0 - (1.0 - t).powi(3);
                    element
                        .opacity(start_opacity * (1.0 - eased))
                        .left(px(lerp(start_x, -5.0, eased)))
                },
            );
        body = body.child(outgoing);
    }
    body.into_any_element()
}

#[derive(Clone, Copy)]
pub enum TabIntent {
    Activate(usize),
    Close(usize),
    ReopenMenu {
        at: gpui::Point<gpui::Pixels>,
    },
    ToggleLock(usize),
    ContextMenu {
        id: usize,
        at: gpui::Point<gpui::Pixels>,
    },
    Reorder {
        session_id: u64,
        source_id: usize,
        source_locked: bool,
        before_id: Option<usize>,
    },
    Swap {
        session_id: u64,
        source_id: usize,
        source_locked: bool,
        target_id: usize,
        target_locked: bool,
    },
    Transfer {
        session_id: u64,
        source_id: usize,
        source_locked: bool,
        before_id: Option<usize>,
    },
    DirectoryDragStarted {
        source_id: usize,
        session_id: u64,
    },
    DirectoryDragEnded {
        source_id: usize,
        session_id: u64,
    },
    New,
}

fn tab_drop_intent(
    candidate: Option<TabDropCandidate>,
    drag: &TabDirectoryDragPayload,
    target_pane_key: u64,
) -> Option<TabIntent> {
    let same_pane = drag.source_pane_key == target_pane_key;
    match (candidate?, same_pane) {
        (TabDropCandidate::Gap { before_id, .. }, true) => Some(TabIntent::Reorder {
            session_id: drag.session_id(),
            source_id: drag.source_id,
            source_locked: drag.source_locked,
            before_id,
        }),
        (
            TabDropCandidate::Swap {
                target_id,
                target_locked,
            },
            true,
        ) => Some(TabIntent::Swap {
            session_id: drag.session_id(),
            source_id: drag.source_id,
            source_locked: drag.source_locked,
            target_id,
            target_locked,
        }),
        (TabDropCandidate::Gap { before_id, .. }, false) => Some(TabIntent::Transfer {
            session_id: drag.session_id(),
            source_id: drag.source_id,
            source_locked: drag.source_locked,
            before_id,
        }),
        (TabDropCandidate::Swap { target_id, .. }, false) => Some(TabIntent::Transfer {
            session_id: drag.session_id(),
            source_id: drag.source_id,
            source_locked: drag.source_locked,
            before_id: Some(target_id),
        }),
    }
}

#[cfg(test)]
mod pane_transfer_intent_tests {
    use super::*;

    fn drag() -> TabDirectoryDragPayload {
        TabDirectoryDragPayload {
            source_id: 7,
            source_pane_key: 1,
            session_id: 9,
            source_locked: false,
            title: SharedString::from("source"),
            location: SharedString::from("C:/source"),
            disambiguation: None,
            locked: false,
            active: true,
            can_close: true,
            row_width: px(220.0),
        }
    }

    #[test]
    fn dropping_into_another_pane_emits_transfer_with_the_target_gap() {
        let intent = tab_drop_intent(
            Some(TabDropCandidate::Gap {
                index: 1,
                before_id: Some(11),
            }),
            &drag(),
            2,
        );
        assert!(matches!(
            intent,
            Some(TabIntent::Transfer {
                session_id: 9,
                source_id: 7,
                source_locked: false,
                before_id: Some(11),
            })
        ));
    }

    #[test]
    fn dropping_into_a_tab_in_another_pane_inserts_before_it() {
        let intent = tab_drop_intent(
            Some(TabDropCandidate::Swap {
                target_id: 11,
                target_locked: false,
            }),
            &drag(),
            2,
        );
        assert!(matches!(
            intent,
            Some(TabIntent::Transfer {
                before_id: Some(11),
                ..
            })
        ));
    }

    #[test]
    fn drops_in_the_source_pane_keep_local_reorder_and_swap_intents() {
        assert!(matches!(
            tab_drop_intent(
                Some(TabDropCandidate::Gap {
                    index: 1,
                    before_id: None,
                }),
                &drag(),
                1,
            ),
            Some(TabIntent::Reorder { .. })
        ));
        assert!(matches!(
            tab_drop_intent(
                Some(TabDropCandidate::Swap {
                    target_id: 11,
                    target_locked: false,
                }),
                &drag(),
                1,
            ),
            Some(TabIntent::Swap { .. })
        ));
    }
}

static NEXT_TAB_DRAG_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn next_tab_drag_session_id() -> u64 {
    loop {
        let id = NEXT_TAB_DRAG_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

#[derive(Clone)]
pub struct TabDirectoryDragPayload {
    source_id: usize,
    source_pane_key: u64,
    session_id: u64,
    source_locked: bool,
    title: SharedString,
    location: SharedString,
    disambiguation: Option<SharedString>,
    locked: bool,
    active: bool,
    can_close: bool,
    row_width: Pixels,
}

impl TabDirectoryDragPayload {
    pub fn source_tab_id(&self) -> usize {
        self.source_id
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }
}

struct TabReorderGhost {
    title: SharedString,
    location: SharedString,
    disambiguation: Option<SharedString>,
    width: Pixels,
    locked: bool,
    active: bool,
    can_close: bool,
}

impl Render for TabReorderGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let foreground = if self.active {
            theme_palette(cx).on_accent
        } else {
            theme_palette(cx).text
        };
        let inset = tab_face_target_inset(self.active);
        let mut tab = div()
            .relative()
            .w(self.width)
            .h(px(tab_row_height(self.locked)))
            .flex()
            .items_center()
            .pl(px(10.0))
            .text_size(px(12.0))
            .text_color(foreground)
            .child(
                div()
                    .absolute()
                    .top(px(inset))
                    .bottom(px(inset))
                    .left(px(0.0))
                    .right(px(0.0))
                    .rounded(px(RADIUS_SM))
                    .bg(if self.active {
                        theme_palette(cx).accent
                    } else {
                        theme_palette(cx).surface_raised
                    }),
            )
            .child(tab_label_body(
                &TabLabel {
                    id: 0,
                    title: self.title.clone(),
                    location: self.location.clone(),
                    locked: self.locked,
                    disambiguation: self.disambiguation.clone(),
                },
                theme_palette(cx),
            ))
            .child(
                div()
                    .size(px(44.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(if self.locked {
                            IconId::Unlock
                        } else {
                            IconId::Lock
                        })
                        .size(px(13.0))
                        .color(foreground.into()),
                    ),
            );
        if self.can_close {
            tab = tab.child(
                div()
                    .size(px(44.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconId::Close)
                            .size(px(12.0))
                            .color(theme_palette(cx).on_accent.into()),
                    ),
            );
        }
        tab.opacity(0.9)
    }
}

fn standard_tab_click_intent(id: usize, click_count: usize) -> TabIntent {
    if click_count == 2 {
        TabIntent::ToggleLock(id)
    } else {
        TabIntent::Activate(id)
    }
}

type TabListener = Rc<dyn Fn(&TabIntent, &mut Window, &mut App)>;
const TAB_REORDER_DRAG_THRESHOLD: f32 = 6.0;

fn is_single_mouse_tab_click(event: &gpui::ClickEvent) -> bool {
    matches!(
        event,
        gpui::ClickEvent::Mouse(mouse)
            if mouse.down.source == gpui::MouseInputSource::Mouse
                && mouse.down.button == gpui::MouseButton::Left
                && mouse.down.click_count == 1
    )
}

fn click_exceeded_tab_reorder_threshold(event: &gpui::ClickEvent) -> bool {
    if !is_single_mouse_tab_click(event) {
        return false;
    }
    let gpui::ClickEvent::Mouse(mouse) = event else {
        unreachable!("the single mouse click predicate was true");
    };
    (mouse.up.position - mouse.down.position).magnitude() > px(TAB_REORDER_DRAG_THRESHOLD).into()
}

#[cfg(test)]
mod tab_click_tests {
    use super::{TabIntent, standard_tab_click_intent};

    #[test]
    fn standard_click_routes_single_and_double_clicks() {
        assert!(matches!(
            standard_tab_click_intent(7, 1),
            TabIntent::Activate(7)
        ));
        assert!(matches!(
            standard_tab_click_intent(7, 2),
            TabIntent::ToggleLock(7)
        ));
    }

    #[test]
    fn click_counts_other_than_two_activate_like_v5() {
        assert!(matches!(
            standard_tab_click_intent(7, 3),
            TabIntent::Activate(7)
        ));
    }
}

#[cfg(test)]
mod vertical_tabs_visual_tests {
    use super::{TabIntent, TabLabel, TabReorderInteraction, VerticalTabs, VerticalTabsText};
    use gpui::{
        Bounds, Context, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent,
        ParentElement, Pixels, Point, Render, ScrollHandle, SharedString, Styled,
        VisualTestContext, Window, div, point, px,
    };
    use std::{cell::RefCell, rc::Rc};

    struct TestTabs(Rc<RefCell<Vec<TabIntent>>>);

    impl Render for TestTabs {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let intents = self.0.clone();
            div().w(px(260.0)).h(px(320.0)).child(VerticalTabs::new(
                vec![
                    TabLabel {
                        id: 7,
                        title: SharedString::from("First"),
                        location: SharedString::from("C:/first"),
                        disambiguation: None,
                        locked: false,
                    },
                    TabLabel {
                        id: 8,
                        title: SharedString::from("Second"),
                        location: SharedString::from("C:/second"),
                        disambiguation: None,
                        locked: false,
                    },
                ],
                0,
                ScrollHandle::new(),
                VerticalTabsText {
                    heading: SharedString::from("Tabs"),
                    show_heading: true,
                    new_tab: SharedString::from("⟦!! 新しいタブ ·· 新しいタブ !!⟧".repeat(4)),
                },
                TabReorderInteraction::default(),
                px(244.0),
                move |intent, _, _| intents.borrow_mut().push(*intent),
            ))
        }
    }

    fn center(bounds: Bounds<Pixels>) -> Point<Pixels> {
        point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn click(cx: &mut VisualTestContext, position: Point<Pixels>, click_count: usize) {
        let modifiers = Modifiers::default();
        cx.simulate_event(MouseDownEvent {
            source: gpui::MouseInputSource::Mouse,
            button: MouseButton::Left,
            position,
            modifiers,
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count,
        });
    }

    fn take_intents(intents: &Rc<RefCell<Vec<TabIntent>>>) -> Vec<TabIntent> {
        std::mem::take(&mut *intents.borrow_mut())
    }

    #[gpui::test]
    fn dispatches_row_and_child_actions_through_hit_testing(cx: &mut gpui::TestAppContext) {
        let intents = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let intents = intents.clone();
            move |_, _| TestTabs(intents)
        });

        let row = cx.debug_bounds("tab-row-7").expect("first tab row bounds");
        let row_point = point(row.origin.x + px(20.0), row.origin.y + px(20.0));
        click(cx, row_point, 1);
        assert!(matches!(
            take_intents(&intents).as_slice(),
            [TabIntent::Activate(7)]
        ));

        click(cx, row_point, 2);
        assert!(matches!(
            take_intents(&intents).as_slice(),
            [TabIntent::ToggleLock(7)]
        ));

        click(cx, row_point, 3);
        assert!(matches!(
            take_intents(&intents).as_slice(),
            [TabIntent::Activate(7)]
        ));

        let lock = cx.debug_bounds("tab-lock-7").expect("lock button bounds");
        click(cx, center(lock), 1);
        assert!(matches!(
            take_intents(&intents).as_slice(),
            [TabIntent::ToggleLock(7)]
        ));

        let close = cx.debug_bounds("tab-close-7").expect("close button bounds");
        click(cx, center(close), 1);
        assert!(matches!(
            take_intents(&intents).as_slice(),
            [TabIntent::Close(7)]
        ));

        cx.simulate_event(MouseDownEvent {
            source: gpui::MouseInputSource::Mouse,
            button: MouseButton::Middle,
            position: row_point,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Middle,
            position: row_point,
            modifiers: Modifiers::default(),
            click_count: 1,
        });
        assert!(matches!(
            take_intents(&intents).as_slice(),
            [TabIntent::Close(7)]
        ));
    }
}

#[derive(IntoElement)]
pub struct VerticalTabs {
    tabs: Vec<TabLabel>,
    active: usize,
    scroll: ScrollHandle,
    text: VerticalTabsText,
    reorder_interaction: TabReorderInteraction,
    row_width: Pixels,
    on_intent: TabListener,
    drop_target_decorator: Option<NavigationDropTargetDecorator>,
    focused_pane: Option<bool>,
    owner_pane_key: u64,
}
impl VerticalTabs {
    pub fn new(
        tabs: Vec<TabLabel>,
        active: usize,
        scroll: ScrollHandle,
        text: VerticalTabsText,
        reorder_interaction: TabReorderInteraction,
        row_width: Pixels,
        on_intent: impl Fn(&TabIntent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            tabs,
            active,
            scroll,
            text,
            reorder_interaction,
            row_width,
            on_intent: Rc::new(on_intent),
            drop_target_decorator: None,
            focused_pane: None,
            owner_pane_key: 0,
        }
    }

    pub fn with_owner_pane_key(mut self, key: u64) -> Self {
        self.owner_pane_key = key;
        self
    }

    pub fn with_focused_pane(mut self, focused: bool) -> Self {
        self.focused_pane = Some(focused);
        self
    }

    /// Application-owned drop presentation can decorate canonical stable tab rows.
    pub fn with_drop_target_decorator(
        mut self,
        decorator: impl Fn(usize, gpui::Div) -> gpui::Div + 'static,
    ) -> Self {
        self.drop_target_decorator = Some(Rc::new(decorator));
        self
    }

    /// Pinned tabsと選択中tabを初期表示できるグループ高を返す。
    pub fn group_height_for_pinned_and_active(
        tabs: &[TabLabel],
        active: usize,
        minimum_height: f32,
        maximum_height: f32,
        show_heading: bool,
    ) -> Pixels {
        let active_index = tabs.iter().position(|tab| tab.id == active);
        let last_pinned_index = tabs.iter().rposition(|tab| tab.locked);
        let last_visible_index = active_index.max(last_pinned_index);
        let visible_rows_height = last_visible_index.map_or(0.0, |last_visible_index| {
            tabs.iter()
                .take(last_visible_index + 1)
                .map(|tab| tab_row_height(tab.locked))
                .sum::<f32>()
        });
        let chrome_height = VERTICAL_TABS_CHROME_HEIGHT
            + if show_heading {
                VERTICAL_TABS_CHILD_GAP + VERTICAL_TABS_HEADING_HEIGHT
            } else {
                0.0
            };
        let required_height = chrome_height + visible_rows_height + VERTICAL_TABS_VISIBLE_ROW_SLACK;
        let maximum_height = maximum_height.max(0.0);
        // 短いウィンドウでも、少なくとも通常タブ1行分のviewportを保つ。
        // それより小さいレールではグループ全体が外側のscroll範囲に入り、行を見失わない。
        let minimum_row_viewport = chrome_height + TAB_REGULAR_ROW_HEIGHT;
        let minimum_height = minimum_height.min(maximum_height).max(minimum_row_viewport);
        px(minimum_height.max(required_height.min(maximum_height)))
    }
}
impl RenderOnce for VerticalTabs {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let VerticalTabs {
            tabs,
            active,
            scroll,
            text,
            reorder_interaction,
            row_width,
            on_intent,
            drop_target_decorator,
            focused_pane,
            owner_pane_key,
        } = self;
        let can_close = tabs.len() > 1;
        let tab_count = tabs.len();
        reorder_interaction.reconcile_label_transitions(&tabs);
        reorder_interaction.reconcile_active_faces(&tabs, active);
        let canonical_tabs: Vec<_> = tabs.iter().map(|tab| (tab.id, tab.locked)).collect();
        reorder_interaction.validate_committed_order(
            &canonical_tabs.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        );
        let mut list = div()
            .id("vertical-tabs")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .relative()
            .flex()
            .flex_col();
        for (index, tab) in tabs.iter().enumerate() {
            let id = tab.id;
            let locked = tab.locked;
            let current_preview = reorder_interaction.preview();
            let is_placeholder = reorder_interaction.is_placeholder(id);
            let is_settling_source = reorder_interaction.is_settling_source(id);
            let is_returning_swap = reorder_interaction.is_returning_swap(id);
            let is_committed_face = reorder_interaction.is_committed_face(id);
            let is_source_slot = is_placeholder || is_settling_source;
            let is_swap_source =
                is_placeholder && matches!(current_preview, Some(TabPreview::Swap(_)));
            let swap_target = match current_preview {
                Some(TabPreview::Swap(target_id)) if is_swap_source => {
                    tabs.iter().find(|candidate| candidate.id == target_id)
                }
                _ => None,
            };
            let shown_tab = swap_target.unwrap_or(tab);
            let shown_id = shown_tab.id;
            let shown_locked = shown_tab.locked;
            let shown_title = shown_tab.title.clone();
            let shown_location = shown_tab.location.clone();
            let shown_disambiguation = shown_tab.disambiguation.clone();
            let title = shown_title.clone();
            let location = shown_location.clone();
            let activate = on_intent.clone();
            let close = on_intent.clone();
            let middle = on_intent.clone();
            let toggle_lock = on_intent.clone();
            let context_menu = on_intent.clone();
            let hover_group = format!("tab-{id}");
            let can_close_tab = can_close && !shown_locked;
            let is_swap_target = current_preview == Some(TabPreview::Swap(id));
            let clear_drag_started = reorder_interaction.clone();
            let mark_drag_started = reorder_interaction.clone();
            let consume_drag_started = reorder_interaction.clone();
            let start_drag_intent = on_intent.clone();
            let (gap_generation, gap_from, gap_to) =
                reorder_interaction.gap_height_animation(index);
            let face_is_active = shown_id == active;
            let active_face_is_focused = face_is_active && focused_pane != Some(false);
            let face_target_inset = tab_face_target_inset(face_is_active);
            let face_transition = reorder_interaction.face_transition(shown_id);
            let face_from_inset =
                face_transition.map_or(face_target_inset, |transition| transition.from_inset);
            let face = div()
                .absolute()
                .top(px(face_from_inset))
                .bottom(px(face_from_inset))
                .left(px(0.0))
                .right(px(0.0))
                .rounded(px(RADIUS_SM))
                .bg(if face_is_active {
                    if active_face_is_focused {
                        theme_palette(cx).accent
                    } else {
                        theme_palette(cx).surface_raised
                    }
                } else {
                    theme_palette(cx).surface_raised
                })
                .when(face_is_active && focused_pane == Some(true), |face| {
                    face.border_1().border_color(theme_palette(cx).on_accent)
                })
                .when(face_is_active && focused_pane == Some(false), |face| {
                    face.border_1().border_color(theme_palette(cx).accent)
                })
                .group_hover(hover_group.clone(), |style| {
                    style.bg(if active_face_is_focused {
                        theme_palette(cx).accent_hover
                    } else {
                        theme_palette(cx).chrome_hover
                    })
                })
                .when(is_swap_target, |face| {
                    face.bg(theme_palette(cx).chrome_hover_strong)
                        .border_2()
                        .border_color(theme_palette(cx).accent)
                })
                .when(is_placeholder && !is_settling_source, |face| {
                    face.opacity(0.0)
                });
            let face = if let Some(transition) = face_transition {
                let from_inset = transition.from_inset;
                let to_inset = transition.to_inset;
                face.with_animation(
                    format!("tab-face-inset-{shown_id}-{}", transition.generation),
                    Animation::new(TAB_PREVIEW_DURATION),
                    move |element, t| {
                        let eased = 1.0 - (1.0 - t).powi(3);
                        let inset = lerp(from_inset, to_inset, eased);
                        element.top(px(inset)).bottom(px(inset))
                    },
                )
                .into_any_element()
            } else {
                face.into_any_element()
            };
            let gap = div()
                .id(("tab-reorder-gap", index))
                .debug_selector(|| format!("tab-reorder-gap-{index}"))
                .w_full()
                .h(px(gap_from))
                .flex_shrink_0()
                .rounded(px(3.0))
                .when(current_preview == Some(TabPreview::Gap(index)), |gap| {
                    gap.bg(theme_palette(cx).accent)
                })
                .with_animation(
                    format!("tab-gap-preview-{index}-{gap_generation}"),
                    Animation::new(TAB_PREVIEW_DURATION),
                    move |element, t| {
                        let eased = 1.0 - (1.0 - t).powi(3);
                        element.h(px(lerp(gap_from, gap_to, eased)))
                    },
                );
            list = list.child(gap);
            let mut row = div();
            if let Some(decorator) = drop_target_decorator.as_ref() {
                row = decorator(id, row);
            }
            let row = row
                .id(("tab", id))
                .debug_selector(|| format!("tab-row-{id}"))
                .relative()
                .group(hover_group.clone())
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .h(px(tab_row_height(locked)))
                .pl(px(10.0))
                .text_color(if active_face_is_focused {
                    theme_palette(cx).on_accent
                } else {
                    theme_palette(cx).text
                })
                .child(face)
                .when(
                    (!is_placeholder || is_settling_source)
                        && !is_swap_target
                        && !is_returning_swap
                        && !is_committed_face,
                    |tab| {
                        tab.child(
                            div()
                                .id(("tab-body", id))
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .child(animated_tab_label_body(
                                    shown_tab,
                                    reorder_interaction.label_transition(shown_id),
                                    theme_palette(cx),
                                )),
                        )
                        // 未 lock 時は hover で現れ、lock 中は状態を読めるよう常に残す。
                        // lock の close 防止・並び順は shell の Tab が所有する。
                        .child(
                            div()
                                .id(("toggle-tab-lock", id))
                                .debug_selector(|| format!("tab-lock-{id}"))
                                .size(px(44.0))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(RADIUS_SM))
                                .opacity(if shown_locked { 1.0 } else { 0.0 })
                                .group_hover(hover_group.clone(), |style| style.opacity(1.0))
                                .hover(|style| style.bg(theme_palette(cx).chrome_hover))
                                .child(
                                    Icon::new(if shown_locked {
                                        IconId::Unlock
                                    } else {
                                        IconId::Lock
                                    })
                                    .size(px(13.0))
                                    .color(
                                        (if id == active {
                                            theme_palette(cx).on_accent
                                        } else {
                                            theme_palette(cx).text
                                        })
                                        .into(),
                                    ),
                                )
                                .on_click(move |event, window, cx| {
                                    cx.stop_propagation();
                                    if event.standard_click() {
                                        toggle_lock(&TabIntent::ToggleLock(id), window, cx);
                                    }
                                }),
                        )
                        .when(can_close_tab, |tab| {
                            tab.child(
                                div()
                                    .id(("close-tab", id))
                                    .debug_selector(|| format!("tab-close-{id}"))
                                    .size(px(44.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .hover(|style| style.bg(theme_palette(cx).close_hover))
                                    .child(
                                        Icon::new(IconId::Close)
                                            .size(px(12.0))
                                            .color(theme_palette(cx).on_accent.into()),
                                    )
                                    .on_click(move |event, window, cx| {
                                        cx.stop_propagation();
                                        if event.standard_click() {
                                            close(&TabIntent::Close(id), window, cx);
                                        }
                                    }),
                            )
                        })
                        .child({
                            div()
                                .id(("tab-drag-handle", id))
                                .absolute()
                                .left(px(0.0))
                                .top(px(0.0))
                                .bottom(px(0.0))
                                .right(px(80.0))
                                .on_drag_with_threshold_when(
                                    TabDirectoryDragPayload {
                                        source_id: id,
                                        source_pane_key: owner_pane_key,
                                        session_id: next_tab_drag_session_id(),
                                        source_locked: locked,
                                        title: title.clone(),
                                        location: location.clone(),
                                        disambiguation: shown_disambiguation.clone(),
                                        locked,
                                        active: id == active,
                                        can_close: can_close_tab,
                                        row_width,
                                    },
                                    px(TAB_REORDER_DRAG_THRESHOLD),
                                    |event| {
                                        event.source == gpui::MouseInputSource::Mouse
                                            && event.button == gpui::MouseButton::Left
                                            && event.click_count == 1
                                    },
                                    move |drag, _, window, cx| {
                                        start_drag_intent(
                                            &TabIntent::DirectoryDragStarted {
                                                source_id: drag.source_tab_id(),
                                                session_id: drag.session_id(),
                                            },
                                            window,
                                            cx,
                                        );
                                        mark_drag_started.mark_drag_started(id, locked);
                                        window.refresh();
                                        cx.new(|_| TabReorderGhost {
                                            title: drag.title.clone(),
                                            location: drag.location.clone(),
                                            disambiguation: drag.disambiguation.clone(),
                                            width: drag.row_width,
                                            locked: drag.locked,
                                            active: drag.active,
                                            can_close: drag.can_close,
                                        })
                                    },
                                )
                        })
                    },
                )
                .on_mouse_down(gpui::MouseButton::Left, move |_, _, _| {
                    // A canceled drag may not produce a row click. Reset on the next contact.
                    clear_drag_started.clear_drag_started();
                })
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    let was_reorder_drag =
                        is_single_mouse_tab_click(event) && consume_drag_started.take_drag_for(id);
                    if was_reorder_drag {
                        window.refresh();
                    }
                    if event.standard_click()
                        && !was_reorder_drag
                        && !click_exceeded_tab_reorder_threshold(event)
                    {
                        activate(
                            &standard_tab_click_intent(id, event.click_count()),
                            window,
                            cx,
                        );
                    }
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                    cx.stop_propagation();
                    context_menu(
                        &TabIntent::ContextMenu {
                            id,
                            at: event.position,
                        },
                        window,
                        cx,
                    );
                })
                .on_mouse_down(gpui::MouseButton::Middle, move |_, window, cx| {
                    cx.stop_propagation();
                    if can_close_tab {
                        middle(&TabIntent::Close(id), window, cx);
                    }
                });
            if is_source_slot {
                let (generation, from_height, to_height) =
                    reorder_interaction.source_height_animation();
                list = list.child(
                    div()
                        .id(("tab-row-height", id))
                        .w_full()
                        .h(px(from_height))
                        .flex_shrink_0()
                        .overflow_hidden()
                        .with_animation(
                            format!("tab-source-preview-{id}-{generation}"),
                            Animation::new(TAB_PREVIEW_DURATION),
                            move |element, t| {
                                let eased = 1.0 - (1.0 - t).powi(3);
                                element.h(px(lerp(from_height, to_height, eased)))
                            },
                        )
                        .child(row),
                );
            } else if is_swap_target {
                let generation = reorder_interaction.source_height_animation().0;
                list = list.child(row.with_animation(
                    format!("tab-swap-dummy-{id}-{generation}"),
                    Animation::new(TAB_PREVIEW_DURATION),
                    |element, t| {
                        let eased = 1.0 - (1.0 - t).powi(3);
                        element.opacity(0.2 + 0.65 * eased)
                    },
                ));
            } else {
                list = list.child(row);
            }
        }
        let end_gap_generation = reorder_interaction.gap_height_animation(tab_count);
        let end_gap = div()
            .id(("tab-reorder-gap", usize::MAX))
            .debug_selector(|| "tab-reorder-gap-end".to_owned())
            .w_full()
            .h(px(end_gap_generation.1))
            .flex_shrink_0()
            .rounded(px(3.0))
            .when(
                reorder_interaction.preview() == Some(TabPreview::Gap(tab_count)),
                |gap| gap.bg(theme_palette(cx).accent),
            )
            .with_animation(
                format!("tab-gap-preview-{tab_count}-{}", end_gap_generation.0),
                Animation::new(TAB_PREVIEW_DURATION),
                move |element, t| {
                    let eased = 1.0 - (1.0 - t).powi(3);
                    element.h(px(lerp(end_gap_generation.1, end_gap_generation.2, eased)))
                },
            );
        list = list.child(end_gap);

        if let (Some(TabPreview::Swap(target_id)), Some(source_id)) = (
            reorder_interaction.preview(),
            reorder_interaction.placeholder_tab.get(),
        ) {
            if let (Some(source_index), Some(target)) = (
                tabs.iter().position(|tab| tab.id == source_id),
                tabs.iter().find(|tab| tab.id == target_id),
            ) {
                let (generation, from_offset, to_offset) =
                    reorder_interaction.swap_face_animation();
                let source_y = reorder_interaction.layout_row_y(
                    source_index,
                    &canonical_tabs,
                    reorder_interaction.preview(),
                );
                list = list.child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(source_y + from_offset))
                        .w_full()
                        .h(px(tab_row_height(target.locked)))
                        .with_animation(
                            format!("tab-swap-face-{source_id}-{target_id}-{generation}"),
                            Animation::new(TAB_PREVIEW_DURATION),
                            move |element, t| {
                                let eased = 1.0 - (1.0 - t).powi(3);
                                element.top(px(source_y + lerp(from_offset, to_offset, eased)))
                            },
                        )
                        .child(tab_projection_face(
                            cx,
                            target.title.clone(),
                            target.location.clone(),
                            target.locked,
                            target.disambiguation.clone(),
                            target.id == active,
                            can_close && !target.locked,
                        )),
                );
            }
        }

        for (generation, returning_id, from_y, to_y) in
            reorder_interaction.returning_swap_animations()
        {
            if let Some(returning) = tabs.iter().find(|tab| tab.id == returning_id) {
                list = list.child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(from_y))
                        .w_full()
                        .h(px(tab_row_height(returning.locked)))
                        .with_animation(
                            format!("tab-swap-return-{returning_id}-{generation}"),
                            Animation::new(TAB_PREVIEW_DURATION),
                            move |element, t| {
                                let eased = 1.0 - (1.0 - t).powi(3);
                                element.top(px(lerp(from_y, to_y, eased)))
                            },
                        )
                        .child(tab_projection_face(
                            cx,
                            returning.title.clone(),
                            returning.location.clone(),
                            returning.locked,
                            returning.disambiguation.clone(),
                            returning.id == active,
                            can_close && !returning.locked,
                        )),
                );
            }
        }

        let (generation, committed_faces) = reorder_interaction.committed_face_animations();
        for (id, from_y, to_y) in committed_faces {
            if let Some(tab) = tabs.iter().find(|tab| tab.id == id) {
                list = list.child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(from_y))
                        .w_full()
                        .h(px(tab_row_height(tab.locked)))
                        .with_animation(
                            format!("tab-reorder-settle-{id}-{generation}"),
                            Animation::new(TAB_PREVIEW_DURATION),
                            move |element, t| {
                                let eased = 1.0 - (1.0 - t).powi(3);
                                element.top(px(lerp(from_y, to_y, eased)))
                            },
                        )
                        .child(tab_projection_face(
                            cx,
                            tab.title.clone(),
                            tab.location.clone(),
                            tab.locked,
                            tab.disambiguation.clone(),
                            tab.id == active,
                            can_close && !tab.locked,
                        )),
                );
            }
        }

        let moving_interaction = reorder_interaction.clone();
        let moving_tabs = canonical_tabs.clone();
        let moving_scroll = scroll.clone();
        list = list.on_drag_move::<TabDirectoryDragPayload>(move |event, window, cx| {
            let drag = event.drag(cx);
            let list_origin_y = event.bounds.origin.y;
            let candidate = tab_candidate_at_pointer(
                event.event.position.y,
                list_origin_y,
                moving_scroll.offset().y,
                drag.source_id,
                drag.source_locked,
                &moving_tabs,
            );
            moving_interaction.update_candidate(
                candidate,
                list_origin_y,
                moving_tabs.len() + 1,
                &moving_tabs,
                window,
            );
        });

        let can_drop_list_origin = reorder_interaction.list_origin_y.clone();
        let can_drop_tabs = canonical_tabs.clone();
        let can_drop_scroll = scroll.clone();
        list = list.can_drop(move |value, window, _| {
            let Some(drag) = value.downcast_ref::<TabDirectoryDragPayload>() else {
                return false;
            };
            let Some(list_origin_y) = can_drop_list_origin.get() else {
                return false;
            };
            let candidate = tab_candidate_at_pointer(
                window.mouse_position().y,
                list_origin_y,
                can_drop_scroll.offset().y,
                drag.source_id,
                drag.source_locked,
                &can_drop_tabs,
            );
            candidate.is_some()
        });

        let drop_list_origin = reorder_interaction.list_origin_y.clone();
        let drop_tabs = canonical_tabs;
        let drop_scroll = scroll;
        let drop_intent = on_intent.clone();
        let drop_end_intent = on_intent.clone();
        let drop_pane_key = owner_pane_key;
        list = list.on_drop::<TabDirectoryDragPayload>(move |drag, window, cx| {
            if let Some(list_origin_y) = drop_list_origin.get() {
                let candidate = tab_candidate_at_pointer(
                    window.mouse_position().y,
                    list_origin_y,
                    drop_scroll.offset().y,
                    drag.source_id,
                    drag.source_locked,
                    &drop_tabs,
                );
                let intent = tab_drop_intent(candidate, drag, drop_pane_key);
                if let Some(intent) = intent {
                    drop_intent(&intent, window, cx);
                }
            }
            drop_end_intent(
                &TabIntent::DirectoryDragEnded {
                    source_id: drag.source_tab_id(),
                    session_id: drag.session_id(),
                },
                window,
                cx,
            );
        });

        let add = on_intent;
        div()
            .size_full()
            .flex()
            .flex_col()
            .p(px(VERTICAL_TABS_OUTER_PADDING))
            .gap(px(VERTICAL_TABS_CHILD_GAP))
            .bg(theme_palette(cx).surface_raised)
            .text_color(theme_palette(cx).text)
            .text_size(px(12.0))
            .when(text.show_heading, |group| {
                group.child(
                    div()
                        .h(px(VERTICAL_TABS_HEADING_HEIGHT))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .child(text.heading),
                )
            })
            .child(list)
            .child(
                div()
                    .id("new-tab")
                    .h(px(VERTICAL_TABS_ACTION_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .rounded(px(RADIUS_SM))
                    .bg(theme_palette(cx).chrome_hover)
                    .hover(|style| style.bg(theme_palette(cx).chrome_hover_strong))
                    .child(
                        div().flex_shrink_0().child(
                            Icon::new(IconId::Add)
                                .size(px(14.0))
                                .color(theme_palette(cx).text.into()),
                        ),
                    )
                    .child(div().min_w_0().truncate().child(text.new_tab))
                    .on_click(move |event, window, cx| {
                        if event.standard_click() {
                            add(&TabIntent::New, window, cx);
                        }
                    }),
            )
    }
}

#[derive(IntoElement)]
pub struct ReopenTabsButton {
    on_intent: TabListener,
}

impl ReopenTabsButton {
    pub fn new(on_intent: impl Fn(&TabIntent, &mut Window, &mut App) + 'static) -> Self {
        Self {
            on_intent: Rc::new(on_intent),
        }
    }
}

impl RenderOnce for ReopenTabsButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let on_intent = self.on_intent;
        let palette = theme_palette(cx);
        div()
            .h(px(VERTICAL_TABS_ACTION_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(palette.surface_raised)
            .child(
                div()
                    .id("reopen-tab")
                    // 見た目は小さいアイコンでも、タッチ対象は44px四方を保つ。
                    .size(px(VERTICAL_TABS_ACTION_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(RADIUS_SM))
                    .hover(|style| style.bg(palette.chrome_hover))
                    .child(
                        Icon::new(IconId::Delete)
                            .size(px(16.0))
                            .color(palette.text.into()),
                    )
                    .on_click(move |event, window, cx| {
                        cx.stop_propagation();
                        if event.standard_click() {
                            on_intent(
                                &TabIntent::ReopenMenu {
                                    at: event.position(),
                                },
                                window,
                                cx,
                            );
                        }
                    }),
            )
    }
}
