//! Local wrapped IME input for inline rename; filesystem ownership stays outside.
use gpui::{
    App, AvailableSpace, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, IntoElement, LayoutId, Pixels,
    Render, Style, TextRun, UTF16Selection, Window, WrappedLine, div, fill, prelude::*, px,
    relative, rgba, size,
};
use std::ops::Range;

gpui::actions!(
    rename_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Home,
        End,
        SelectLeft,
        SelectRight,
        SelectAll,
        UndoText,
        CopyText,
        PasteText
    ]
);

pub struct RenameInput {
    focus: FocusHandle,
    text: String,
    selection: Range<usize>,
    selection_reversed: bool,
    pointer_selection_anchor: Option<usize>,
    marked: Option<Range<usize>>,
    /// IME の取消しで戻す、preedit開始前の選択範囲と文字列。
    ///
    /// TSF は選択範囲を preedit で置き換えたあと、Escape を空文字の
    /// replacement として通知することがある。marked だけを消すと、元の
    /// filename まで消えてしまうため、composition中だけ保持する。
    marked_restore: Option<(String, Range<usize>, bool)>,
    undo_history: Vec<TextUndo>,
    bounds: Option<Bounds<Pixels>>,
    hit_layout: Option<HitLayout>,
    expected_height: Option<Pixels>,
    centered: bool,
    /// address bar は折り返さない。Rename だけが item row に合わせて複数行になる。
    single_line: bool,
}

struct TextUndo {
    prior_text: String,
    prior_selection: Range<usize>,
    prior_selection_reversed: bool,
}

#[derive(Clone)]
struct HitLayout {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    carets: Vec<(usize, gpui::Point<Pixels>)>,
}

impl HitLayout {
    fn closest_index(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        let local = position - self.bounds.origin;
        let max_line = self
            .carets
            .iter()
            .map(|(_, point)| (point.y / self.line_height).floor().max(0.0) as usize)
            .max()?;
        let target_line = ((local.y / self.line_height).floor().max(0.0) as usize).min(max_line);
        self.carets
            .iter()
            .filter(|(_, point)| {
                (point.y / self.line_height).floor().max(0.0) as usize == target_line
            })
            .min_by(|(_, a), (_, b)| {
                (a.x - local.x)
                    .abs()
                    .as_f32()
                    .total_cmp(&(b.x - local.x).abs().as_f32())
            })
            .map(|(index, _)| *index)
    }

    fn caret_for_index(&self, index: usize) -> Option<gpui::Point<Pixels>> {
        self.carets
            .iter()
            .find_map(|(candidate, point)| (*candidate == index).then_some(*point))
    }
}

pub enum RenameInputEvent {
    TextChanged,
    CopyPlainText(String),
}

fn filename_clipboard_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn plain_text_from_clipboard(item: gpui::ClipboardItem) -> Option<String> {
    let mut found_text = false;
    let text = item
        .into_entries()
        .filter_map(|entry| match entry {
            gpui::ClipboardEntry::String(value) => {
                found_text = true;
                Some(value.into_text())
            }
            gpui::ClipboardEntry::Image(_) | gpui::ClipboardEntry::ExternalPaths(_) => None,
        })
        .collect::<String>();
    found_text.then_some(text)
}

impl gpui::EventEmitter<RenameInputEvent> for RenameInput {}

/// Explorer と同じく、file は最後の拡張子の直前までを初期選択する。
/// dotfile と拡張子の無い名前は全体を選ぶ。folder か設定で含める場合も全体。
pub fn initial_filename_selection(text: &str, exclude_extension: bool) -> Range<usize> {
    if !exclude_extension {
        return 0..text.len();
    }
    let end = std::path::Path::new(text)
        .extension()
        .filter(|extension| !extension.is_empty())
        .and_then(|extension| {
            let extension_len = extension.to_string_lossy().len();
            text.len().checked_sub(extension_len + 1)
        })
        .filter(|end| *end > 0)
        .unwrap_or(text.len());
    0..end
}

impl RenameInput {
    pub fn new(
        cx: &mut Context<Self>,
        text: String,
        centered: bool,
        initial_selection: Range<usize>,
    ) -> Self {
        debug_assert!(text.is_char_boundary(initial_selection.start));
        debug_assert!(text.is_char_boundary(initial_selection.end));
        debug_assert!(initial_selection.start <= initial_selection.end);
        debug_assert!(initial_selection.end <= text.len());
        Self {
            focus: cx.focus_handle(),
            text,
            selection: initial_selection,
            selection_reversed: false,
            pointer_selection_anchor: None,
            marked: None,
            marked_restore: None,
            undo_history: Vec::new(),
            bounds: None,
            hit_layout: None,
            expected_height: None,
            centered,
            single_line: false,
        }
    }

    /// Rename と同じ TSF/IME・UTF-16・caret の経路を使う address-bar 用の一行 editor。
    /// 別実装へ分けると、以前直した IME の二重確定やクリック位置の不整合を再発させやすい。
    pub fn address_bar(cx: &mut Context<Self>, text: String) -> Self {
        let selection = 0..text.len();
        Self {
            focus: cx.focus_handle(),
            text,
            selection,
            selection_reversed: false,
            pointer_selection_anchor: None,
            marked: None,
            marked_restore: None,
            undo_history: Vec::new(),
            bounds: None,
            hit_layout: None,
            expected_height: None,
            centered: false,
            single_line: true,
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn is_composing(&self) -> bool {
        self.marked.is_some()
    }
    /// 親が描画前に同じ text system で求めた折返し高。
    ///
    /// 初回 paint 後に bounds から高さを取り直すと、一度だけ親の stretch 高で
    /// 描いてから縮む。inline rename はこの値を先に受け取って揺れを防ぐ。
    pub fn set_expected_height(&mut self, height: Pixels) {
        self.expected_height = Some(height.max(px(1.0)));
    }
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx)
    }
    pub fn focus_at(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_marked_composition_as_undo();
        self.focus(window, cx);
        if let Some(byte) = self
            .hit_layout
            .as_ref()
            .and_then(|layout| layout.closest_index(position))
        {
            self.selection = byte..byte;
            self.selection_reversed = false;
            self.pointer_selection_anchor = None;
            self.marked = None;
            cx.notify();
        }
    }

    pub fn begin_pointer_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_at(position, window, cx);
        self.pointer_selection_anchor = self
            .hit_layout
            .as_ref()
            .and_then(|layout| layout.closest_index(position));
    }

    pub fn extend_pointer_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(anchor) = self.pointer_selection_anchor else {
            return false;
        };
        let Some(cursor) = self
            .hit_layout
            .as_ref()
            .and_then(|layout| layout.closest_index(position))
        else {
            return false;
        };
        self.commit_marked_composition_as_undo();
        self.selection = anchor.min(cursor)..anchor.max(cursor);
        self.selection_reversed = cursor < anchor;
        self.marked = None;
        cx.notify();
        true
    }

    pub fn end_pointer_selection(&mut self) {
        self.pointer_selection_anchor = None;
    }
    fn shape(&self, width: Pixels, window: &Window) -> Option<WrappedLine> {
        let style = window.text_style();
        let mut runs = Vec::with_capacity(3);
        let mut push_run = |len: usize, selected: bool| {
            if len > 0 {
                runs.push(TextRun {
                    len,
                    font: style.font(),
                    color: style.color,
                    background_color: selected.then(|| rgba(0x662d7dff).into()),
                    underline: None,
                    strikethrough: None,
                });
            }
        };
        push_run(self.selection.start, false);
        push_run(
            self.selection.end.saturating_sub(self.selection.start),
            true,
        );
        push_run(self.text.len().saturating_sub(self.selection.end), false);
        drop(push_run);
        if runs.is_empty() {
            runs.push(TextRun {
                len: 0,
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
        window
            .text_system()
            .shape_text(
                self.text.clone().into(),
                style.font_size.to_pixels(window.rem_size()),
                &runs,
                (!self.single_line).then_some(width.max(px(1.0))),
                None,
            )
            .ok()?
            .into_iter()
            .next()
    }
    fn required_height(&self, width: Pixels, window: &Window) -> Pixels {
        if self.single_line {
            return window.line_height();
        }
        self.shape(width, window)
            .map_or(window.line_height(), |line| {
                line.size(window.line_height()).height
            })
            .max(window.line_height())
    }
    fn text_align(&self) -> gpui::TextAlign {
        if self.centered {
            gpui::TextAlign::Center
        } else {
            gpui::TextAlign::Left
        }
    }
    fn line_index_for_y(line: &WrappedLine, y: Pixels, line_height: Pixels) -> usize {
        if line_height <= Pixels::ZERO {
            return 0;
        }
        ((y / line_height).floor().max(0.0) as usize).min(line.wrap_boundaries().len())
    }
    fn line_end(line: &WrappedLine, line_index: usize) -> usize {
        line.wrap_boundaries()
            .get(line_index)
            .and_then(|boundary| {
                line.runs()
                    .get(boundary.run_ix)
                    .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                    .map(|glyph| glyph.index)
            })
            .unwrap_or_else(|| line.len())
    }
    fn line_alignment_offset(
        &self,
        line: &WrappedLine,
        line_index: usize,
        line_height: Pixels,
        align_width: Pixels,
    ) -> Pixels {
        if !self.centered {
            return Pixels::ZERO;
        }
        let end = Self::line_end(line, line_index);
        let width = line
            .position_for_index(end, line_height)
            .map_or(Pixels::ZERO, |position| position.x);
        ((align_width - width) / 2.0).max(Pixels::ZERO)
    }
    fn aligned_position(
        &self,
        line: &WrappedLine,
        index: usize,
        line_height: Pixels,
        align_width: Pixels,
    ) -> Option<gpui::Point<Pixels>> {
        let boundary_line =
            line.wrap_boundaries()
                .iter()
                .enumerate()
                .find_map(|(line_index, boundary)| {
                    line.runs()
                        .get(boundary.run_ix)
                        .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                        .is_some_and(|glyph| glyph.index == index)
                        .then_some(line_index + 1)
                });
        let mut position = if let Some(line_index) = boundary_line {
            gpui::point(Pixels::ZERO, line_height * line_index as f32)
        } else {
            line.position_for_index(index, line_height)?
        };
        let line_index =
            boundary_line.unwrap_or_else(|| Self::line_index_for_y(line, position.y, line_height));
        position.x += self.line_alignment_offset(line, line_index, line_height, align_width);
        Some(position)
    }
    pub fn backspace(&mut self, cx: &mut Context<Self>) {
        let prior_selection = self.selection.clone();
        let prior_selection_reversed = self.selection_reversed;
        let history_len = self.undo_history.len();
        if self.selection.is_empty() {
            let start = self.text[..self.selection.start]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i);
            self.selection = start..self.selection.end;
        }
        self.replace(None, "", cx);
        if self.undo_history.len() > history_len
            && let Some(undo) = self.undo_history.last_mut()
        {
            undo.prior_selection = prior_selection;
            undo.prior_selection_reversed = prior_selection_reversed;
        }
    }
    pub fn delete_forward(&mut self, cx: &mut Context<Self>) {
        let prior_selection = self.selection.clone();
        let prior_selection_reversed = self.selection_reversed;
        let history_len = self.undo_history.len();
        if self.selection.is_empty() && self.selection.end < self.text.len() {
            let end = self.text[self.selection.end..]
                .char_indices()
                .nth(1)
                .map_or(self.text.len(), |(i, _)| self.selection.end + i);
            self.selection = self.selection.start..end;
        }
        self.replace(None, "", cx);
        if self.undo_history.len() > history_len
            && let Some(undo) = self.undo_history.last_mut()
        {
            undo.prior_selection = prior_selection;
            undo.prior_selection_reversed = prior_selection_reversed;
        }
    }
    pub fn undo_text(&mut self, cx: &mut Context<Self>) {
        // IME preedit は確定済み編集ではなく、ここでは履歴を進めない。
        if self.marked.is_some() {
            return;
        }
        let Some(undo) = self.undo_history.pop() else {
            return;
        };
        if undo.prior_selection.start > undo.prior_selection.end
            || undo.prior_selection.end > undo.prior_text.len()
            || !undo.prior_text.is_char_boundary(undo.prior_selection.start)
            || !undo.prior_text.is_char_boundary(undo.prior_selection.end)
        {
            self.undo_history.clear();
            return;
        }
        self.text = undo.prior_text;
        self.selection = undo.prior_selection;
        self.selection_reversed = undo.prior_selection_reversed;
        self.hit_layout = None;
        cx.emit(RenameInputEvent::TextChanged);
        cx.notify();
    }
    fn commit_marked_composition_as_undo(&mut self) {
        if self.marked.take().is_none() {
            return;
        }
        if let Some((text, range, selection_reversed)) = self.marked_restore.take() {
            self.undo_history.push(TextUndo {
                prior_text: text,
                prior_selection: range,
                prior_selection_reversed: selection_reversed,
            });
        } else {
            self.undo_history.clear();
        }
    }
    pub fn move_cursor(&mut self, key: &str, extend: bool, cx: &mut Context<Self>) {
        self.commit_marked_composition_as_undo();
        let head = if self.selection_reversed {
            self.selection.start
        } else {
            self.selection.end
        };
        let anchor = if self.selection_reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        let cursor = match key {
            "left" if !extend && !self.selection.is_empty() => self.selection.start,
            "right" if !extend && !self.selection.is_empty() => self.selection.end,
            "left" => self.text[..head]
                .char_indices()
                .last()
                .map_or(0, |(i, _)| i),
            "right" => self.text[head..]
                .char_indices()
                .nth(1)
                .map_or(self.text.len(), |(i, _)| head + i),
            "home" => 0,
            "end" => self.text.len(),
            _ => return,
        };
        if extend {
            self.selection = anchor.min(cursor)..anchor.max(cursor);
            self.selection_reversed = cursor < anchor;
        } else {
            self.selection = cursor..cursor;
            self.selection_reversed = false;
        }
        self.marked = None;
        cx.notify();
    }
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.commit_marked_composition_as_undo();
        self.selection = 0..self.text.len();
        self.selection_reversed = false;
        self.marked = None;
        cx.notify();
    }
    fn from16(&self, o: usize) -> usize {
        let (mut a, mut b) = (0, 0);
        for c in self.text.chars() {
            if b >= o {
                break;
            }
            b += c.len_utf16();
            a += c.len_utf8()
        }
        a
    }
    fn to16(&self, o: usize) -> usize {
        let (mut a, mut b) = (0, 0);
        for c in self.text.chars() {
            if a >= o {
                break;
            }
            a += c.len_utf8();
            b += c.len_utf16()
        }
        b
    }

    fn restore_marked_composition(&mut self, cx: &mut Context<Self>) -> bool {
        if self.marked.is_none() {
            return false;
        }
        let Some((restore_text, restore_range, selection_reversed)) = self.marked_restore.take()
        else {
            return false;
        };
        self.marked = None;
        self.text = restore_text;
        self.selection = restore_range;
        self.selection_reversed = selection_reversed;
        self.hit_layout = None;
        cx.emit(RenameInputEvent::TextChanged);
        cx.notify();
        true
    }

    fn rfrom(&self, r: &Range<usize>) -> Range<usize> {
        self.from16(r.start)..self.from16(r.end)
    }
    fn rto(&self, r: &Range<usize>) -> Range<usize> {
        self.to16(r.start)..self.to16(r.end)
    }
    fn from16_in(value: &str, offset: usize) -> usize {
        let (mut utf8, mut utf16) = (0, 0);
        for c in value.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += c.len_utf16();
            utf8 += c.len_utf8();
        }
        utf8
    }
    fn replace(&mut self, r: Option<Range<usize>>, v: &str, cx: &mut Context<Self>) {
        // Windows TSF の確定は `unmark` の後に range=None で届くことがある。
        // 変換中 range を先に take してから replacement の fallback に使わないと、
        // preedit の後ろへ確定文字列を追加してしまう。
        if v.is_empty() && self.restore_marked_composition(cx) {
            return;
        }
        let marked = self.marked.take();
        let composition_restore = self.marked_restore.take();
        let r = r.or(marked).unwrap_or_else(|| self.selection.clone());
        let (prior_selection, prior_text, prior_selection_reversed) = composition_restore
            .map(|(restore_text, restore_range, selection_reversed)| {
                (restore_range, restore_text, selection_reversed)
            })
            .unwrap_or_else(|| {
                (
                    r.clone(),
                    self.text.clone(),
                    self.selection_reversed,
                )
            });
        self.text.replace_range(r.clone(), v);
        self.hit_layout = None;
        let p = r.start + v.len();
        self.selection = p..p;
        self.selection_reversed = false;
        if self.text != prior_text {
            self.undo_history.push(TextUndo {
                prior_text,
                prior_selection,
                prior_selection_reversed,
            });
        }
        cx.emit(RenameInputEvent::TextChanged);
        cx.notify()
    }
}
impl Focusable for RenameInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EntityInputHandler for RenameInput {
    fn text_for_range(
        &mut self,
        r: Range<usize>,
        a: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let r = self.rfrom(&r);
        a.replace(self.rto(&r));
        Some(self.text[r].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.rto(&self.selection),
            reversed: self.selection_reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.rto(r))
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        // Windows TSF can send `unmark` immediately before the final
        // `replace_text_in_range(None, committed)`. Keep the composition
        // range until that replacement consumes it; clearing here would append
        // the committed text beside the preedit text.
    }
    fn replace_text_in_range(
        &mut self,
        r: Option<Range<usize>>,
        v: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(r.as_ref().map(|r| self.rfrom(r)), v, cx)
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        r: Option<Range<usize>>,
        v: &str,
        s: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if v.is_empty() && self.restore_marked_composition(cx) {
            return;
        }
        let r = r
            .as_ref()
            .map(|r| self.rfrom(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let marked_restore = self.marked_restore.clone().or_else(|| {
            Some((self.text.clone(), r.clone(), self.selection_reversed))
        });
        self.text.replace_range(r.clone(), v);
        self.hit_layout = None;
        self.marked = (!v.is_empty()).then_some(r.start..r.start + v.len());
        self.marked_restore = if self.marked.is_some() {
            marked_restore
        } else {
            None
        };
        self.selection = s
            .map(|s| Self::from16_in(v, s.start)..Self::from16_in(v, s.end))
            .map(|s| r.start + s.start..r.start + s.end)
            .unwrap_or_else(|| r.start + v.len()..r.start + v.len());
        self.selection_reversed = false;
        cx.emit(RenameInputEvent::TextChanged);
        cx.notify()
    }
    fn bounds_for_range(
        &mut self,
        r: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let r = self.rfrom(&r);
        let layout = self.hit_layout.as_ref()?;
        let start = layout.caret_for_index(r.start)?;
        Some(Bounds::new(
            layout.bounds.origin + start,
            size(px(1.0), layout.line_height),
        ))
    }
    fn character_index_for_point(
        &mut self,
        p: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.hit_layout.as_ref()?.closest_index(p)?;
        Some(self.to16(index.min(self.text.len())))
    }
}
impl Render for RenameInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 最初の paint で取得した実幅を以後の layout に使う。custom element の
        // request_layout は親から既知 height を渡されることがあるため、ここで
        // content height を明示しないと親の高さへ stretch される。
        let content_height = self.expected_height.or_else(|| {
            self.bounds
                .map(|bounds| self.required_height(bounds.size.width, window))
        });
        div()
            .w_full()
            .min_w_0()
            // 親の cell は同じ virtual row の高さへ stretch される。editor まで
            // stretch させると content height を二重に数えるので、自身の測定高を使う。
            .self_start()
            .when_some(content_height, |editor, height| editor.h(height))
            .min_h(if self.single_line {
                window.line_height()
            } else {
                window.line_height() * 2.0
            })
            .flex()
            .items_center()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.begin_pointer_selection(event.position, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(gpui::MouseButton::Left)
                    && this.extend_pointer_selection(event.position, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_pointer_selection();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_pointer_selection();
                    cx.stop_propagation();
                }),
            )
            .key_context("RenameInput")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &UndoText, _, cx| this.undo_text(cx)))
            .on_action(cx.listener(|this, _: &Backspace, _, cx| this.backspace(cx)))
            .on_action(cx.listener(|this, _: &Delete, _, cx| this.delete_forward(cx)))
            .on_action(cx.listener(|this, _: &Left, _, cx| this.move_cursor("left", false, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.move_cursor("right", false, cx)))
            .on_action(cx.listener(|this, _: &Home, _, cx| this.move_cursor("home", false, cx)))
            .on_action(cx.listener(|this, _: &End, _, cx| this.move_cursor("end", false, cx)))
            .on_action(
                cx.listener(|this, _: &SelectLeft, _, cx| this.move_cursor("left", true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectRight, _, cx| this.move_cursor("right", true, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &CopyText, _, cx| {
                if this.marked.is_none() && !this.selection.is_empty() {
                    cx.emit(RenameInputEvent::CopyPlainText(
                        this.text[this.selection.clone()].to_owned(),
                    ));
                }
            }))
            .on_action(cx.listener(|this, _: &PasteText, _, cx| {
                if this.marked.is_some() {
                    return;
                }
                let Some(text) = cx.read_from_clipboard().and_then(plain_text_from_clipboard)
                else {
                    return;
                };
                let plain = filename_clipboard_text(&text);
                if plain.is_empty() && !text.is_empty() {
                    return;
                }
                this.replace(None, &plain, cx);
            }))
            .child(RenameInputElement::new(cx.entity()))
    }
}
pub struct RenameInputElement {
    input: Entity<RenameInput>,
}
impl RenameInputElement {
    pub fn new(input: Entity<RenameInput>) -> Self {
        Self { input }
    }
}
impl IntoElement for RenameInputElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
pub struct Paint {
    line: WrappedLine,
    cursor: Option<gpui::PaintQuad>,
    /// 親の行が編集欄より高くても、文字列と IME の基準は中央に置く。
    bounds: Bounds<Pixels>,
    hit_layout: HitLayout,
}
impl Element for RenameInputElement {
    type RequestLayoutState = ();
    type PrepaintState = Paint;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        w: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut s = Style::default();
        s.size.width = relative(1.).into();
        let input = self.input.clone();
        let layout = w.request_measured_layout(s, move |known, available, window, cx| {
            let width = known
                .width
                .or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                })
                .unwrap_or(px(1.0))
                .max(px(1.0));
            // 親の flex line が持つ高さをここへ渡すと、editor がその高さへ stretch
            // して次の計測でもさらに高くなる。入力欄自身は常に text の必要高だけを
            // 返し、行の高さは外側の virtual layout が一度だけ決める。
            let height = input.read(cx).required_height(width, window);
            size(width, height.max(window.line_height()))
        });
        (layout, ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        b: Bounds<Pixels>,
        _: &mut (),
        w: &mut Window,
        cx: &mut App,
    ) -> Paint {
        let i = self.input.read(cx);
        let l = i.shape(b.size.width, w).unwrap_or_default();
        // 行の高さは仮想化側が持つ。長い名前を編集中にその行だけ余る場合でも、
        // 文字列、caret、IME の候補位置が一緒に上下中央へ寄るよう描画領域を絞る。
        let text_height = if i.single_line {
            w.line_height()
        } else {
            l.size(w.line_height()).height.max(w.line_height())
        };
        let vertical_inset = ((b.size.height - text_height) / 2.0).max(Pixels::ZERO);
        let bounds = Bounds::new(
            gpui::point(b.origin.x, b.origin.y + vertical_inset),
            size(b.size.width, text_height),
        );
        let cursor = if i.selection.is_empty() {
            i.aligned_position(&l, i.selection.end, w.line_height(), bounds.size.width)
                .map(|position| {
                    fill(
                        Bounds::new(bounds.origin + position, size(px(2.), w.line_height())),
                        rgba(0x99ffffff),
                    )
                })
        } else {
            None
        };
        let carets = i
            .text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(i.text.len()))
            .filter_map(|index| {
                i.aligned_position(&l, index, w.line_height(), bounds.size.width)
                    .map(|point| (index, point))
            })
            .collect();
        Paint {
            line: l,
            cursor,
            bounds,
            hit_layout: HitLayout {
                bounds,
                line_height: w.line_height(),
                carets,
            },
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        s: &mut Paint,
        w: &mut Window,
        cx: &mut App,
    ) {
        let (f, align) = {
            let input = self.input.read(cx);
            (input.focus.clone(), input.text_align())
        };
        w.handle_input(
            &f,
            ElementInputHandler::new(s.bounds, self.input.clone()),
            cx,
        );
        s.line
            .paint_background(
                s.bounds.origin,
                w.line_height(),
                align,
                Some(s.bounds),
                w,
                cx,
            )
            .unwrap();
        s.line
            .paint(
                s.bounds.origin,
                w.line_height(),
                align,
                Some(s.bounds),
                w,
                cx,
            )
            .unwrap();
        if f.is_focused(w)
            && let Some(q) = s.cursor.take()
        {
            w.paint_quad(q)
        }
        self.input.update(cx, |i, _| {
            i.bounds = Some(s.bounds);
            i.hit_layout = Some(s.hit_layout.clone());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HitLayout, RenameInput, filename_clipboard_text, initial_filename_selection,
        plain_text_from_clipboard,
    };
    use gpui::{
        AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Render, Styled,
        Subscription, div, px, size,
    };
    use std::{cell::RefCell, rc::Rc};

    struct ClipboardRouteHarness {
        input: Entity<RenameInput>,
        copied: Rc<RefCell<Vec<String>>>,
        text_changes: Rc<RefCell<usize>>,
        subscription: Option<Subscription>,
    }

    impl Render for ClipboardRouteHarness {
        fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.subscription.is_none() {
                let copied = self.copied.clone();
                let text_changes = self.text_changes.clone();
                self.subscription = Some(cx.subscribe(
                    &self.input,
                    move |_, _, event, _| match event {
                        super::RenameInputEvent::CopyPlainText(text) => {
                            copied.borrow_mut().push(text.clone());
                        }
                        super::RenameInputEvent::TextChanged => {
                            *text_changes.borrow_mut() += 1;
                        }
                    },
                ));
            }
            div().size_full().child(self.input.clone())
        }
    }

    #[gpui::test]
    fn focused_ctrl_c_and_v_route_to_shared_editor_and_replace_unicode_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| {
            app.bind_keys([
                gpui::KeyBinding::new("ctrl-c", super::CopyText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-v", super::PasteText, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-z", super::UndoText, Some("RenameInput")),
            ]);
        });
        let copied = Rc::new(RefCell::new(Vec::new()));
        let text_changes = Rc::new(RefCell::new(0));
        let copied_for_view = copied.clone();
        let text_changes_for_view = text_changes.clone();
        let (harness, visual) = cx.add_window_view(move |_, cx| {
            let value = "日本語tail".to_owned();
            let selected_end = "日本語".len();
            let input = cx.new(|cx| RenameInput::new(cx, value, false, 0..selected_end));
            ClipboardRouteHarness {
                input,
                copied: copied_for_view,
                text_changes: text_changes_for_view,
                subscription: None,
            }
        });
        let input = visual.read_entity(&harness, |harness, _| harness.input.clone());
        visual.update(|window, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string("😀\r\n".to_owned()));
            input.update(app, |input, cx| input.focus(window, cx));
        });

        visual.simulate_keystrokes("ctrl-c");
        assert_eq!(&*copied.borrow(), &["日本語"]);
        visual.simulate_keystrokes("ctrl-v");
        visual.read_entity(&input, |input, _| assert_eq!(input.text(), "😀tail"));
        visual.simulate_keystrokes("ctrl-z");
        visual.read_entity(&input, |input, _| {
            assert_eq!(input.text(), "日本語tail");
            assert_eq!(input.selection, 0.."日本語".len());
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.read_entity(&input, |input, _| assert_eq!(input.text(), "日本語tail"));

        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string("kept".to_owned()));
        });
        visual.update_entity(&input, |input, _| {
            input.selection = 0..0;
        });
        visual.simulate_keystrokes("ctrl-c");
        assert_eq!(
            visual.update(|_, app| app.read_from_clipboard().unwrap().text()),
            Some("kept".into())
        );
        assert_eq!(copied.borrow().len(), 1);

        let mut external_paths = gpui::ExternalPaths::default();
        external_paths
            .0
            .push(std::path::PathBuf::from("C:\\fixture\\file.txt"));
        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem {
                entries: vec![gpui::ClipboardEntry::ExternalPaths(external_paths)],
            });
        });
        visual.simulate_keystrokes("ctrl-v");
        visual.read_entity(&input, |input, _| assert_eq!(input.text(), "日本語tail"));
        assert_eq!(*text_changes.borrow(), 2);

        visual.update(|_, app| {
            app.write_to_clipboard(gpui::ClipboardItem::new_string("ignored".to_owned()));
        });
        visual.update_entity(&input, |input, _| {
            input.marked = Some(0..4);
        });
        visual.simulate_keystrokes("ctrl-v");
        visual.read_entity(&input, |input, _| assert_eq!(input.text(), "日本語tail"));
        assert_eq!(*text_changes.borrow(), 2);
    }

    #[gpui::test]
    fn selecting_during_short_multibyte_preedit_keeps_undo_ranges_valid(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|app| {
            app.bind_keys([
                gpui::KeyBinding::new("ctrl-a", super::SelectAll, Some("RenameInput")),
                gpui::KeyBinding::new("ctrl-z", super::UndoText, Some("RenameInput")),
            ]);
        });
        let copied = Rc::new(RefCell::new(Vec::new()));
        let text_changes = Rc::new(RefCell::new(0));
        let copied_for_view = copied.clone();
        let text_changes_for_view = text_changes.clone();
        let (harness, visual) = cx.add_window_view(move |_, cx| {
            let input = cx.new(|cx| RenameInput::new(cx, "abcdefghij".to_owned(), false, 0..10));
            ClipboardRouteHarness {
                input,
                copied: copied_for_view,
                text_changes: text_changes_for_view,
                subscription: None,
            }
        });
        let input = visual.read_entity(&harness, |harness, _| harness.input.clone());
        visual.update(|window, app| {
            input.update(app, |input, cx| {
                input.focus(window, cx);
                input.replace(Some(0..10), "0123456789", cx);
                gpui::EntityInputHandler::replace_and_mark_text_in_range(
                    input,
                    Some(0..10),
                    "あ",
                    None,
                    window,
                    cx,
                );
            });
        });
        visual.read_entity(&input, |input, _| assert_eq!(input.text(), "あ"));

        visual.update_entity(&input, |input, cx| {
            assert!(!input.extend_pointer_selection(gpui::point(px(0.0), px(0.0)), cx));
            assert_eq!(input.text(), "あ");
            assert!(input.marked.is_some());
            assert!(input.marked_restore.is_some());
        });

        // Selection abandons the active composition as one undoable text edit.
        visual.simulate_keystrokes("ctrl-a");
        visual.read_entity(&input, |input, _| {
            assert_eq!(input.text(), "あ");
            assert!(input.marked.is_none());
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.read_entity(&input, |input, _| {
            assert_eq!(input.text(), "0123456789");
            assert_eq!(input.selection, 0..10);
        });
        visual.simulate_keystrokes("ctrl-z");
        visual.read_entity(&input, |input, _| {
            assert_eq!(input.text(), "abcdefghij");
            assert_eq!(input.selection, 0..10);
        });
        visual.update_entity(&input, |input, cx| {
            input.undo_history.push(super::TextUndo {
                prior_text: "safe".to_owned(),
                prior_selection: 0..20,
                prior_selection_reversed: false,
            });
            input.undo_text(cx);
            assert_eq!(input.text(), "abcdefghij");
            assert!(input.undo_history.is_empty());
        });

        visual.update(|window, app| {
            input.update(app, |input, cx| {
                input.selection = 1..10;
                input.selection_reversed = true;
                gpui::EntityInputHandler::replace_and_mark_text_in_range(
                    input,
                    Some(1..10),
                    "あ",
                    None,
                    window,
                    cx,
                );
            });
        });
        visual.update_entity(&input, |input, cx| {
            assert_eq!(input.text(), "aあ");
            assert!(input.restore_marked_composition(cx));
            assert_eq!(input.text(), "abcdefghij");
            assert_eq!(input.selection, 1..10);
            assert!(input.selection_reversed);
        });
    }

    #[test]
    fn ime_relative_utf16_selection_keeps_astral_boundaries() {
        assert_eq!(RenameInput::from16_in("😀あ", 2), 4);
        assert_eq!(RenameInput::from16_in("😀あ", 3), "😀".len() + "あ".len());
    }

    #[test]
    fn clipboard_paste_keeps_unicode_and_drops_filename_controls() {
        assert_eq!(filename_clipboard_text("日本語😀 name"), "日本語😀 name");
        assert_eq!(filename_clipboard_text("before\r\nafter\0"), "beforeafter");
    }

    #[test]
    fn file_only_clipboard_has_no_plain_text_payload() {
        let mut external_paths = gpui::ExternalPaths::default();
        external_paths
            .0
            .push(std::path::PathBuf::from("C:\\fixture\\file.txt"));
        let item = gpui::ClipboardItem {
            entries: vec![gpui::ClipboardEntry::ExternalPaths(external_paths)],
        };
        assert_eq!(plain_text_from_clipboard(item), None);
    }

    #[test]
    fn initial_selection_excludes_only_a_real_final_extension() {
        assert_eq!(initial_filename_selection("report.txt", true), 0..6);
        assert_eq!(initial_filename_selection("archive.tar.gz", true), 0..11);
        assert_eq!(initial_filename_selection("日本語.画像", true), 0..9);
        assert_eq!(initial_filename_selection(".gitignore", true), 0..10);
        assert_eq!(initial_filename_selection("README", true), 0..6);
        assert_eq!(initial_filename_selection("report.txt", false), 0..10);
    }

    #[test]
    fn painted_hit_layout_uses_global_bounds_and_the_clicked_wrapped_line() {
        let layout = HitLayout {
            bounds: Bounds::new(gpui::point(px(100.0), px(200.0)), size(px(80.0), px(40.0))),
            line_height: px(20.0),
            carets: vec![
                (0, gpui::point(px(0.0), px(0.0))),
                (3, gpui::point(px(30.0), px(0.0))),
                (6, gpui::point(px(0.0), px(20.0))),
                (9, gpui::point(px(45.0), px(20.0))),
            ],
        };

        assert_eq!(
            layout.closest_index(gpui::point(px(129.0), px(205.0))),
            Some(3)
        );
        assert_eq!(
            layout.closest_index(gpui::point(px(103.0), px(225.0))),
            Some(6)
        );
        assert_eq!(
            layout.closest_index(gpui::point(px(190.0), px(225.0))),
            Some(9)
        );
    }
}
