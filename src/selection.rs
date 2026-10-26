use crate::entry_identity::EntryKey;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Bound;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SectionKey {
    ModifiedDate(ModifiedDateSection),
    Kind(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModifiedDateSection {
    Unknown,
    VeryOld,
    LastYear,
    ThisYear,
    LastMonth,
    ThisMonth,
    LastWeek,
    ThisWeek,
    Yesterday,
    Today,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusTarget {
    Entry(usize),
    Section(SectionKey),
}

pub enum SelectionIntent {
    Checkbox {
        index: usize,
    },
    Pick {
        index: usize,
        toggle: bool,
        range: bool,
    },
    Clear,
    /// section heading からの一括選択。heading 自体は index を持たない。
    SetRange {
        start: usize,
        end: usize,
        selected: bool,
    },
    /// 矢印キーでのフォーカス移動。選択の扱いは現在のモードで変わる。
    MoveFocus {
        index: usize,
        /// Shift 付き。アンカーからの範囲を保留として置き直す。
        extend: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeSelectionState {
    None,
    Some,
    All,
}

/// 余白からの矩形選択が、開始時の選択集合へどう作用するか。modifier は pointer-down
/// 時に `DirectoryListDemo` が固定して渡し、move/upで読み直さない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RectangleMode {
    // 現在のUI bindingはAdd/Toggleだけを使う。Replaceは状態機械の基準動作テスト用。
    #[cfg(test)]
    Replace,
    Toggle,
    Add,
}

/// 矩形の hit は行方向で連続する。巨大なディレクトリで毎 move ごとに BTreeSet を
/// 作らず、baseline と interval だけで表示上の選択を計算する。
#[derive(Clone, Debug)]
struct RectanglePreview {
    baseline: BTreeSet<usize>,
    mode: RectangleMode,
}

/// フォーカスに到達した由来。Shift+矢印のアンカーを範囲に含めるかどうかを決める。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FocusOrigin {
    /// 選択意図を伴う操作で到達した(クリック、通常モードの矢印移動)。
    #[default]
    SelectionIntent,
    /// 選択モード中の通常矢印移動。選択意図がないので、次の Shift で自身も範囲に入る。
    NavigationOnly,
}

/// 表示上の選択は `selected XOR preview`。
///
/// v5 の `temp_xor_selection` と同じ考えで、Shift の範囲は確定させず保留にする。
/// 表示だけが変わり、次の Shift で置き換え、他の操作の直前に確定する。
#[derive(Default)]
pub struct Selection {
    selected: BTreeSet<usize>,
    /// Shift 範囲の保留分。
    preview: BTreeSet<usize>,
    rectangle: Option<RectanglePreview>,
    /// Shift 範囲の起点。通常のクリックでだけ更新し、Shift+クリックでは動かさない。
    anchor: Option<usize>,
    /// 矢印キーが動かす現在位置。選択とは独立に持つ。
    focus: Option<FocusTarget>,
    /// 直前にフォーカスへ到達した由来。
    focus_origin: FocusOrigin,
    /// Shift+矢印のセッション中だけ入るアンカー。
    shift_anchor: Option<(usize, FocusOrigin)>,
    /// M で明示した選択モード。選択が変わると件数に合わせて更新する。
    selection_mode: bool,
}

/// 並べ替えや再列挙の直前に退避する、key で表した選択状態。
///
/// index のまま並びを交換すると、選択が別の項目を指してしまう。交換の直前に
/// これを取り、新しい並びから index へ戻す。
///
/// 移動履歴の bookmark にもこの値をそのまま入れる。訪問先へ戻ったときに、選択と
/// フォーカスをそのまま再現するため。
#[derive(Default, Clone, Debug, PartialEq)]
pub struct SelectionSnapshot {
    selected: Vec<EntryKey>,
    anchor: Option<EntryKey>,
    focus: Option<FocusSnapshot>,
    focus_origin: FocusOrigin,
    selection_mode: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum FocusSnapshot {
    Entry(EntryKey),
    Section(SectionKey),
}

impl SelectionSnapshot {
    pub(crate) fn parts(
        &self,
    ) -> (
        &[EntryKey],
        Option<&EntryKey>,
        Option<&EntryKey>,
        FocusOrigin,
    ) {
        let (focus, origin) = match self.focus.as_ref() {
            Some(FocusSnapshot::Entry(key)) => (Some(key), self.focus_origin),
            Some(FocusSnapshot::Section(_)) => (None, FocusOrigin::SelectionIntent),
            None => (None, self.focus_origin),
        };
        (&self.selected, self.anchor.as_ref(), focus, origin)
    }

    pub(crate) fn from_parts(
        selected: Vec<EntryKey>,
        anchor: Option<EntryKey>,
        focus: Option<EntryKey>,
        focus_origin: FocusOrigin,
    ) -> Self {
        Self {
            selected,
            anchor,
            focus: focus.map(FocusSnapshot::Entry),
            focus_origin,
            selection_mode: false,
        }
    }

    pub(crate) fn from_parts_with_mode(
        selected: Vec<EntryKey>,
        anchor: Option<EntryKey>,
        focus: Option<EntryKey>,
        focus_origin: FocusOrigin,
        selection_mode: bool,
    ) -> Self {
        Self {
            selected,
            anchor,
            focus: focus.map(FocusSnapshot::Entry),
            focus_origin,
            selection_mode,
        }
    }

    pub(crate) fn selection_mode(&self) -> bool {
        self.selection_mode
    }

    /// filesystem reconciliation で同じ項目の identity が変わる場合だけ、退避中の
    /// 選択を新しい key へ引き継ぐ。
    pub fn remap_key(&mut self, old: &EntryKey, new: &EntryKey) {
        for key in &mut self.selected {
            if key == old {
                *key = new.clone();
            }
        }
        if self.anchor.as_ref() == Some(old) {
            self.anchor = Some(new.clone());
        }
        if matches!(self.focus.as_ref(), Some(FocusSnapshot::Entry(key)) if key == old) {
            self.focus = Some(FocusSnapshot::Entry(new.clone()));
        }
    }
}

impl Selection {
    /// Collect a bounded slice of displayed indices in effective selection order.
    /// The cursor advances across the union of committed and preview indices,
    /// including indices cancelled by XOR, so one turn never scans an unbounded
    /// run of overlapping ranges.
    pub fn effective_indices_batch(
        &self,
        after: Option<usize>,
        budget: usize,
        count: usize,
    ) -> (Vec<usize>, Option<usize>, bool) {
        let lower = after.map_or(Bound::Unbounded, Bound::Excluded);
        let mut selected = self.selected.range((lower, Bound::Unbounded)).peekable();
        let lower = after.map_or(Bound::Unbounded, Bound::Excluded);
        let mut preview = self.preview.range((lower, Bound::Unbounded)).peekable();
        let mut indices = Vec::new();
        let mut cursor = after;
        let mut scanned = 0;

        while scanned < budget {
            let next = match (selected.peek(), preview.peek()) {
                (Some(left), Some(right)) if left == right => {
                    let index = **left;
                    selected.next();
                    preview.next();
                    index
                }
                (Some(left), Some(right)) if left < right => *selected.next().unwrap(),
                (Some(_), Some(_)) => *preview.next().unwrap(),
                (Some(_), None) => *selected.next().unwrap(),
                (None, Some(_)) => *preview.next().unwrap(),
                (None, None) => break,
            };
            cursor = Some(next);
            scanned += 1;
            if next < count && (self.selected.contains(&next) ^ self.preview.contains(&next)) {
                indices.push(next);
            }
        }

        let done = selected.peek().is_none() && preview.peek().is_none();
        (indices, cursor, done)
    }

    /// Commit pending selection changes and inspect only keys that will enter a snapshot.
    pub fn snapshot_preflight(
        &mut self,
        mut estimate_key: impl FnMut(usize) -> Result<usize, crate::session_store::SessionError>,
    ) -> Result<(usize, usize), crate::session_store::SessionError> {
        self.commit_transient();
        self.shift_anchor = None;
        let mut count = 0usize;
        let mut bytes = 0usize;
        for index in &self.selected {
            count = count.saturating_add(1);
            bytes = bytes.saturating_add(estimate_key(*index)?);
        }
        for index in self.anchor.into_iter().chain(self.focused()) {
            bytes = bytes.saturating_add(estimate_key(index)?);
        }
        Ok((count, bytes))
    }

    /// 現在の選択・フォーカスを key で退避する。
    ///
    /// 保留中の Shift 範囲はここで確定する。並びをまたぐ Shift session は
    /// 意味を保てないので終了する。
    pub fn snapshot(&mut self, keys: &[EntryKey]) -> SelectionSnapshot {
        self.snapshot_with(|index| keys.get(index).cloned())
    }

    /// Take a key snapshot without first cloning every row key in a large directory.
    pub fn snapshot_with(
        &mut self,
        mut key_at: impl FnMut(usize) -> Option<EntryKey>,
    ) -> SelectionSnapshot {
        self.commit_transient();
        self.shift_anchor = None;
        SelectionSnapshot {
            selected: self
                .selected
                .iter()
                .filter_map(|index| key_at(*index))
                .collect(),
            anchor: self.anchor.and_then(&mut key_at),
            focus: match &self.focus {
                Some(FocusTarget::Entry(index)) => key_at(*index).map(FocusSnapshot::Entry),
                Some(FocusTarget::Section(key)) => Some(FocusSnapshot::Section(key.clone())),
                None => None,
            },
            focus_origin: self.focus_origin,
            selection_mode: self.selection_mode,
        }
    }

    /// 退避した選択を新しい並びへ戻す。
    ///
    /// 同じ key の候補が一意でないときは選択を移さない。消えた項目の選択を
    /// 隣の行へ移さない。消えた focus と anchor も同様に落とす。
    ///
    /// 設計では「消えた focus を旧位置に近い生存行へ置ける」としているが、
    /// 実装はまだそこまでやらない。次の矢印移動は `focused().unwrap_or(0)` で
    /// 先頭へ入るので、操作は続けられる。
    pub fn restore(&mut self, snapshot: SelectionSnapshot, keys: &[EntryKey]) {
        self.restore_inner(snapshot, keys, false);
    }

    /// 同じ場所で並びや表示を作り直すとき、明示モードも保つ。
    pub fn restore_remapped(&mut self, snapshot: SelectionSnapshot, keys: &[EntryKey]) {
        self.restore_inner(snapshot, keys, true);
    }

    fn restore_inner(
        &mut self,
        snapshot: SelectionSnapshot,
        keys: &[EntryKey],
        preserve_mode: bool,
    ) {
        let snapshot_mode = snapshot.selection_mode;
        let mut index_of: HashMap<&EntryKey, usize> = HashMap::with_capacity(keys.len());
        let mut ambiguous: HashSet<&EntryKey> = HashSet::new();
        for (index, key) in keys.iter().enumerate() {
            if index_of.insert(key, index).is_some() {
                // 同じ key が複数ある並びでは、どれを指すか決められない。
                ambiguous.insert(key);
            }
        }
        let resolve = |key: &EntryKey| -> Option<usize> {
            if ambiguous.contains(key) {
                return None;
            }
            index_of.get(key).copied()
        };

        self.selected = snapshot
            .selected
            .iter()
            .filter_map(resolve)
            .collect::<BTreeSet<_>>();
        self.preview.clear();
        self.rectangle = None;
        self.anchor = snapshot.anchor.as_ref().and_then(resolve);
        self.focus = match snapshot.focus {
            Some(FocusSnapshot::Entry(key)) => resolve(&key).map(FocusTarget::Entry),
            Some(FocusSnapshot::Section(key)) if preserve_mode => Some(FocusTarget::Section(key)),
            _ => None,
        };
        self.focus_origin = snapshot.focus_origin;
        self.shift_anchor = None;
        if preserve_mode {
            self.selection_mode = snapshot_mode;
        } else {
            self.ensure_mode();
        }
    }
}

impl Selection {
    /// 互換用の実選択数の判定。名前にかかわらず明示モードとは別。
    pub fn is_multiple(&self) -> bool {
        self.len() > 1
    }

    pub fn is_selection_mode(&self) -> bool {
        self.selection_mode
    }

    pub fn toggle_selection_mode(&mut self) {
        self.selection_mode = !self.selection_mode;
    }

    fn ensure_mode(&mut self) {
        self.selection_mode = self.len() > 1;
    }

    pub fn contains(&self, index: usize) -> bool {
        self.selected.contains(&index) ^ self.preview.contains(&index)
    }

    /// 矢印キーが動かす現在位置。
    pub fn focused(&self) -> Option<usize> {
        match self.focus.as_ref() {
            Some(FocusTarget::Entry(index)) => Some(*index),
            _ => None,
        }
    }

    pub fn focus_target(&self) -> Option<&FocusTarget> {
        self.focus.as_ref()
    }

    pub fn focus_section(&mut self, key: SectionKey) {
        self.commit_transient();
        self.shift_anchor = None;
        self.focus = Some(FocusTarget::Section(key));
        self.focus_origin = FocusOrigin::NavigationOnly;
    }

    /// Shift navigation may pass a heading, but the heading is never a range endpoint.
    /// Keep the entry anchor and pending XOR range until focus reaches another entry.
    pub fn extend_focus_section(&mut self, key: SectionKey) {
        if self.shift_anchor.is_none()
            && let Some(index) = self.focused()
        {
            self.shift_anchor = Some((index, self.focus_origin));
        }
        self.focus = Some(FocusTarget::Section(key));
    }

    pub fn clear_section_focus(&mut self) {
        if matches!(self.focus.as_ref(), Some(FocusTarget::Section(_))) {
            self.focus = None;
            self.shift_anchor = None;
        }
    }

    pub fn toggle_section_members(&mut self, members: std::ops::Range<usize>) {
        self.commit_transient();
        let select = members.clone().any(|index| !self.selected.contains(&index));
        for index in members {
            if select {
                self.selected.insert(index);
            } else {
                self.selected.remove(&index);
            }
        }
        self.shift_anchor = None;
        self.ensure_mode();
    }

    /// 全 displayed entry を選択し、一時的な range / rectangle 状態を終わらせる。
    pub fn select_all(&mut self, count: usize) {
        self.cancel_rectangle();
        if count == 0 {
            self.ensure_mode();
            return;
        }
        self.selected = (0..count).collect();
        self.preview.clear();
        self.shift_anchor = None;
        let focus = self.focused().filter(|index| *index < count).unwrap_or(0);
        self.focus = Some(FocusTarget::Entry(focus));
        self.anchor = Some(focus);
        self.focus_origin = FocusOrigin::SelectionIntent;
        self.ensure_mode();
    }

    /// 表示上の選択数。保留分を織り込んだ数になる。
    pub fn len(&self) -> usize {
        let mut count = self.selected.len() as isize;
        for index in &self.preview {
            if self.selected.contains(index) {
                count -= 1;
            } else {
                count += 1;
            }
        }
        count.max(0) as usize
    }

    pub fn apply(&mut self, intent: SelectionIntent, count: usize) {
        self.cancel_rectangle();
        match intent {
            SelectionIntent::Checkbox { index } => {
                if index < count {
                    self.commit_preview();
                    toggle(&mut self.selected, index);
                    self.anchor = Some(index);
                    self.focus = Some(FocusTarget::Entry(index));
                    self.focus_origin = FocusOrigin::SelectionIntent;
                    self.shift_anchor = None;
                    self.ensure_mode();
                }
            }
            SelectionIntent::Clear => {
                self.selected.clear();
                self.preview.clear();
                self.anchor = None;
                self.focus = None;
                self.focus_origin = FocusOrigin::SelectionIntent;
                self.shift_anchor = None;
                self.ensure_mode();
            }
            SelectionIntent::SetRange {
                start,
                end,
                selected,
            } => {
                if start >= end || start >= count {
                    return;
                }
                self.commit_preview();
                let end = end.min(count);
                if selected {
                    self.selected.extend(start..end);
                    self.anchor = Some(start);
                    self.focus = Some(FocusTarget::Entry(start));
                } else {
                    for index in start..end {
                        self.selected.remove(&index);
                    }
                    if self
                        .focused()
                        .is_some_and(|index| index >= start && index < end)
                    {
                        self.focus = self
                            .selected
                            .iter()
                            .next_back()
                            .copied()
                            .map(FocusTarget::Entry);
                    }
                }
                self.focus_origin = FocusOrigin::SelectionIntent;
                self.shift_anchor = None;
                self.ensure_mode();
            }
            SelectionIntent::Pick {
                index,
                toggle: toggled,
                range,
            } => {
                if index >= count {
                    return;
                }
                if range {
                    // アンカーは動かさない。前回の保留は捨てて置き換える。
                    let from = self.anchor.filter(|from| *from < count);
                    self.set_preview(index, from);
                } else {
                    self.commit_preview();
                    if toggled {
                        toggle(&mut self.selected, index);
                    } else {
                        // 通常モードのクリックは単一選択に揃える(v5 の FocusElement)。
                        self.selected.clear();
                        self.selected.insert(index);
                    }
                    self.anchor = Some(index);
                }
                self.focus = Some(FocusTarget::Entry(index));
                self.focus_origin = FocusOrigin::SelectionIntent;
                self.shift_anchor = None;
                self.ensure_mode();
            }
            SelectionIntent::MoveFocus { index, extend } => {
                if index >= count {
                    return;
                }
                if extend {
                    self.extend_focus(index, count);
                    self.ensure_mode();
                } else {
                    self.move_focus(index);
                }
            }
        }
    }

    /// section checkbox の三状態表示。preview は操作の前に commit されるため、ここでは
    ///確定選択だけを数える。
    pub fn range_selection_state(&self, start: usize, end: usize) -> RangeSelectionState {
        let count = self.selected.range(start..end).count();
        if count == 0 {
            RangeSelectionState::None
        } else if count == end.saturating_sub(start) {
            RangeSelectionState::All
        } else {
            RangeSelectionState::Some
        }
    }

    /// Shift previewを一度確定してから矩形transactionを始める。baselineはここでだけ
    /// cloneするため、drag中のmoveは交差する行だけを更新すればよい。
    pub fn begin_rectangle(&mut self, mode: RectangleMode) {
        self.commit_preview();
        self.shift_anchor = None;
        self.rectangle = Some(RectanglePreview {
            baseline: self.selected.clone(),
            mode,
        });
    }

    /// marquee の geometry は view が持つ。可視 item ごとの実矩形との交差だけを渡すので、
    /// gap や他の列を一つの連続index範囲へ誤変換しない。
    pub fn rectangle_contains(&self, index: usize, hit: bool) -> bool {
        let Some(rectangle) = &self.rectangle else {
            return self.contains(index);
        };
        match rectangle.mode {
            #[cfg(test)]
            RectangleMode::Replace => hit,
            RectangleMode::Toggle => rectangle.baseline.contains(&index) ^ hit,
            RectangleMode::Add => rectangle.baseline.contains(&index) || hit,
        }
    }

    /// Escape / generation changeでは baseline をそのまま残す。確定とは異なり focus/
    /// anchorも触らないので、古いdragが新しいlistingへ選択を移すことはない。
    pub fn cancel_rectangle(&mut self) {
        self.rectangle = None;
    }

    pub fn rectangle_active(&self) -> bool {
        self.rectangle.is_some()
    }

    /// 矢印キーでのフォーカス移動。
    ///
    /// 選択モード中は選択を変えずに `NavigationOnly` を記録し、後続の Shift+矢印が
    /// この位置も範囲に入れられるようにする。
    ///
    /// モードの判定は保留を確定した後の件数で行う。v5 は確定前の `mode` を見ていた
    /// ため、Shift+矢印で範囲を作った直後に矢印を押すと範囲が1件に潰れていた。
    /// ここはユーザーの期待に合わせて意図的に変えている。
    fn move_focus(&mut self, index: usize) {
        self.commit_preview();
        self.shift_anchor = None;
        self.focus = Some(FocusTarget::Entry(index));
        if self.is_selection_mode() {
            self.focus_origin = FocusOrigin::NavigationOnly;
        } else {
            self.selected.clear();
            self.selected.insert(index);
            self.anchor = Some(index);
            self.focus_origin = FocusOrigin::SelectionIntent;
            self.ensure_mode();
        }
    }

    /// Shift+矢印。アンカーは動かさず、範囲を保留として置き直す。
    ///
    /// アンカーが `NavigationOnly` 由来のときはアンカー自身も範囲に含める。
    /// v5 の `include_anchor` と同じ規則。
    fn extend_focus(&mut self, index: usize, count: usize) {
        let previous = self.focused().unwrap_or(index);
        let (anchor, origin) = *self
            .shift_anchor
            .get_or_insert((previous, self.focus_origin));
        self.focus = Some(FocusTarget::Entry(index));
        let include_anchor = origin == FocusOrigin::NavigationOnly;
        self.set_preview_with_anchor(index, anchor, include_anchor, count);
        self.focus_origin = FocusOrigin::SelectionIntent;
    }

    /// Shift 範囲を保留として置き直す。アンカー自身は範囲に含めない。
    fn set_preview(&mut self, index: usize, from: Option<usize>) {
        self.preview.clear();
        let Some(from) = from else {
            self.preview.insert(index);
            return;
        };
        for item in from.min(index)..=from.max(index) {
            if item != from {
                self.preview.insert(item);
            }
        }
    }

    /// Shift+矢印用。アンカーを範囲に含めるかを選べる。
    fn set_preview_with_anchor(
        &mut self,
        index: usize,
        anchor: usize,
        include_anchor: bool,
        count: usize,
    ) {
        self.preview.clear();
        let (start, end) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        for item in start..=end.min(count.saturating_sub(1)) {
            if !include_anchor && item == anchor {
                continue;
            }
            self.preview.insert(item);
        }
    }

    /// 保留分を確定する。
    fn commit_preview(&mut self) {
        let had_preview = !self.preview.is_empty();
        let preview = std::mem::take(&mut self.preview);
        for index in preview {
            toggle(&mut self.selected, index);
        }
        if had_preview {
            self.ensure_mode();
        }
    }

    fn commit_transient(&mut self) {
        self.commit_preview();
        // 実hit geometry は view にしか無い。ここへ届くのは別の selection action / snapshot
        // なので、未知の矩形を連続rangeとして誤確定せず baseline へ戻す。
        self.cancel_rectangle();
    }

    /// rectangleを終了した時だけ、view が一度だけ求めた実hit集合へ落とす。
    pub(super) fn commit_rectangle_hits(&mut self, hits: impl IntoIterator<Item = usize>) {
        let Some(rectangle) = self.rectangle.take() else {
            return;
        };
        let hits = hits.into_iter().collect::<BTreeSet<_>>();
        let mut selected = rectangle.baseline;
        match rectangle.mode {
            #[cfg(test)]
            RectangleMode::Replace => {
                selected = hits.clone();
            }
            RectangleMode::Toggle => {
                for index in &hits {
                    toggle(&mut selected, *index);
                }
            }
            RectangleMode::Add => selected.extend(hits.iter().copied()),
        }
        self.selected = selected;
        self.preview.clear();
        self.shift_anchor = None;
        if let Some(first) = hits.first().copied() {
            self.anchor = Some(first);
            self.focus = Some(FocusTarget::Entry(first));
            self.focus_origin = FocusOrigin::SelectionIntent;
        }
        self.ensure_mode();
    }
}

fn toggle(set: &mut BTreeSet<usize>, index: usize) {
    if !set.insert(index) {
        set.remove(&index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::EntryKind;

    fn pick(index: usize, toggle: bool, range: bool) -> SelectionIntent {
        SelectionIntent::Pick {
            index,
            toggle,
            range,
        }
    }

    #[test]
    fn shift_range_stays_pending_until_the_next_action() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        assert_eq!(state.selected, BTreeSet::from([2]));
        assert_eq!(state.anchor, Some(2));

        // Shift の範囲は表示だけを変え、確定しない。
        state.apply(pick(5, false, true), 10);
        assert_eq!(state.selected, BTreeSet::from([2]));
        assert_eq!(state.len(), 4);
        assert!(state.contains(3) && state.contains(5));
        // Shift はアンカーを動かさない。
        assert_eq!(state.anchor, Some(2));

        // 次の Shift は前回の保留を捨てて置き換える。
        // 「2 を選んで Shift+5、Shift+3」は「2 を選んで Shift+3」と同じ表示になる。
        state.apply(pick(3, false, true), 10);
        assert_eq!(state.selected, BTreeSet::from([2]));
        assert_eq!(state.len(), 2);
        assert!(state.contains(3));
        assert!(!state.contains(5));
        assert_eq!(state.anchor, Some(2));

        // 通常のクリックは保留を確定してから単一選択へ置き換える。
        state.apply(pick(8, false, false), 10);
        assert_eq!(state.selected, BTreeSet::from([8]));
        assert_eq!(state.len(), 1);
        assert_eq!(state.anchor, Some(8));
    }

    #[test]
    fn another_action_commits_the_pending_range() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        state.apply(pick(5, false, true), 10);

        // Ctrl クリックで保留が確定し、そのうえでトグルされる。
        state.apply(pick(7, true, false), 10);
        assert_eq!(state.selected, BTreeSet::from([2, 3, 4, 5, 7]));
        assert_eq!(state.len(), 5);
    }

    #[test]
    fn checkbox_toggles_without_changing_other_entries() {
        let mut state = Selection::default();
        for index in [1, 2, 1] {
            state.apply(pick(index, true, false), 10);
        }
        assert_eq!(state.selected, BTreeSet::from([2]));
        state.apply(pick(20, false, true), 10);
        assert_eq!(state.anchor, Some(1));
        state.apply(SelectionIntent::Clear, 10);
        assert_eq!(state.len(), 0);
        state.apply(pick(4, true, false), 10);
        state.apply(pick(6, false, false), 10);
        assert_eq!(state.selected, BTreeSet::from([6]));
    }

    #[test]
    fn rectangle_preview_keeps_a_large_interval_virtual_until_commit() {
        let mut state = Selection::default();
        state.apply(pick(7, false, false), 100_000);
        state.apply(SelectionIntent::Checkbox { index: 90_000 }, 100_000);

        state.begin_rectangle(RectangleMode::Replace);
        assert!(state.rectangle_contains(10_000, true));
        assert!(state.rectangle_contains(79_999, true));
        assert!(!state.rectangle_contains(80_000, false));

        // move のたびに 70,000 件の集合を作らず、baseline と interval だけで表示する。
        // baseline は残るが、70,000 件の range はまだ selected へ materialize しない。
        assert_eq!(state.selected, BTreeSet::from([7, 90_000]));
        assert_eq!(state.len(), 2, "drag中のchromeはbaselineのまま");

        state.commit_rectangle_hits(10_000..80_000);
        assert_eq!(state.selected.len(), 70_000);
        assert_eq!(state.focused(), Some(10_000));
    }

    #[test]
    fn rectangle_modifiers_use_the_captured_baseline() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 20);
        state.apply(SelectionIntent::Checkbox { index: 4 }, 20);

        state.begin_rectangle(RectangleMode::Toggle);
        assert!(state.rectangle_contains(2, false));
        assert!(state.rectangle_contains(3, true));
        assert!(!state.rectangle_contains(4, true));
        assert!(state.rectangle_contains(5, true));
        state.cancel_rectangle();
        assert_eq!(state.selected, BTreeSet::from([2, 4]));

        state.begin_rectangle(RectangleMode::Add);
        state.commit_rectangle_hits(3..6);
        assert_eq!(state.selected, BTreeSet::from([2, 3, 4, 5]));
    }

    #[test]
    fn mode_follows_the_selection_count() {
        let mut state = Selection::default();
        state.apply(pick(1, false, false), 10);
        assert_eq!(state.selected, BTreeSet::from([1]));
        assert!(!state.is_multiple());

        // 2件目で選択モードに入る(v5 の ensure_mode と同じ規則)。
        state.apply(SelectionIntent::Checkbox { index: 2 }, 10);
        assert_eq!(state.selected, BTreeSet::from([1, 2]));
        assert!(state.is_multiple());

        // 1件に減ると通常モードへ戻る。
        state.apply(SelectionIntent::Checkbox { index: 2 }, 10);
        assert_eq!(state.selected, BTreeSet::from([1]));
        assert!(!state.is_multiple());

        state.apply(SelectionIntent::Clear, 10);
        assert_eq!(state.len(), 0);
        assert!(!state.is_multiple());
        assert_eq!(state.anchor, None);
    }

    #[test]
    fn plain_pick_replaces_the_selection_and_stays_in_normal_mode() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        state.apply(SelectionIntent::Checkbox { index: 5 }, 10);
        assert!(state.is_multiple());

        // 通常モードのクリックは単一選択へ戻す(v5 の FocusElement)。
        state.apply(pick(7, false, false), 10);
        assert_eq!(state.selected, BTreeSet::from([7]));
        assert!(!state.is_multiple());
    }

    #[test]
    fn invalid_checkbox_is_ignored() {
        let mut state = Selection::default();
        state.apply(SelectionIntent::Checkbox { index: 10 }, 10);
        assert!(!state.is_multiple());
        assert_eq!(state.len(), 0);
    }

    #[test]
    fn section_range_reports_none_some_and_all_and_can_toggle() {
        let mut state = Selection::default();
        assert_eq!(state.range_selection_state(3, 6), RangeSelectionState::None);
        state.apply(
            SelectionIntent::SetRange {
                start: 3,
                end: 6,
                selected: true,
            },
            10,
        );
        assert_eq!(state.range_selection_state(3, 6), RangeSelectionState::All);
        state.apply(SelectionIntent::Checkbox { index: 4 }, 10);
        assert_eq!(state.range_selection_state(3, 6), RangeSelectionState::Some);
        state.apply(
            SelectionIntent::SetRange {
                start: 3,
                end: 6,
                selected: false,
            },
            10,
        );
        assert_eq!(state.range_selection_state(3, 6), RangeSelectionState::None);
    }

    fn move_focus(index: usize, extend: bool) -> SelectionIntent {
        SelectionIntent::MoveFocus { index, extend }
    }

    #[test]
    fn plain_focus_move_selects_the_focused_item_in_normal_mode() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        state.apply(move_focus(3, false), 10);
        // 通常モードの矢印移動は単一選択をフォーカスへ揃える。
        assert_eq!(state.selected, BTreeSet::from([3]));
        assert_eq!(state.focused(), Some(3));
        assert_eq!(state.anchor, Some(3));
        assert!(!state.is_multiple());
    }

    #[test]
    fn plain_focus_move_keeps_the_selection_in_multiple_mode() {
        let mut state = Selection::default();
        state.apply(pick(1, false, false), 10);
        state.apply(SelectionIntent::Checkbox { index: 4 }, 10);
        assert!(state.is_multiple());

        state.apply(move_focus(5, false), 10);
        // 選択モード中の矢印移動は選択を変えない。
        assert_eq!(state.selected, BTreeSet::from([1, 4]));
        assert_eq!(state.focused(), Some(5));
    }

    #[test]
    fn shift_arrow_previews_the_range_from_the_focus() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        state.apply(move_focus(5, true), 10);
        // アンカー(2)は範囲に含めない。v5 の include_anchor=false と同じ。
        assert_eq!(state.selected, BTreeSet::from([2]));
        assert_eq!(state.len(), 4);
        assert!(state.contains(3) && state.contains(5));
        assert_eq!(state.focused(), Some(5));

        // 連続した Shift+矢印は同じアンカーから置き直す。
        state.apply(move_focus(3, true), 10);
        assert_eq!(state.len(), 2);
        assert!(state.contains(3) && !state.contains(5));
    }

    #[test]
    fn shift_arrow_after_a_navigation_move_includes_the_anchor() {
        let mut state = Selection::default();
        state.apply(pick(1, false, false), 10);
        state.apply(SelectionIntent::Checkbox { index: 4 }, 10);
        // 選択モード中の矢印移動は NavigationOnly になる。
        state.apply(move_focus(5, false), 10);
        state.apply(move_focus(7, true), 10);
        // 移動前の 5 も範囲に入る。v5 の include_anchor=true と同じ。
        assert!(state.contains(5) && state.contains(6) && state.contains(7));
        // アンカーは XOR 対象なので、既に選択済みの 4 は範囲に入らない。
        assert_eq!(state.selected, BTreeSet::from([1, 4]));
        assert!(!state.preview.contains(&4));
    }

    #[test]
    fn focus_move_keeps_a_committed_range_in_multiple_mode() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        state.apply(pick(5, false, true), 10);
        assert_eq!(state.len(), 4);

        state.apply(move_focus(8, false), 10);
        // 保留を確定し、2件以上なら選択モードを維持する。
        // v5 は確定前のモードを見ていたためここで1件に潰れていたが、
        // Shift+矢印の範囲が矢印移動で消えないよう意図的に変えている。
        assert_eq!(state.selected, BTreeSet::from([2, 3, 4, 5]));
        assert!(state.is_multiple());
        assert_eq!(state.focused(), Some(8));
    }

    #[test]
    fn focus_move_collapses_a_single_selection() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        state.apply(move_focus(8, false), 10);
        // 1件だけなら従来どおりフォーカス位置へ揃える。
        assert_eq!(state.selected, BTreeSet::from([8]));
        assert_eq!(state.focused(), Some(8));
    }

    #[test]
    fn focus_survives_a_clear_but_resets_to_none() {
        let mut state = Selection::default();
        state.apply(pick(2, false, false), 10);
        assert_eq!(state.focused(), Some(2));
        state.apply(SelectionIntent::Clear, 10);
        assert_eq!(state.focused(), None);
    }

    fn keys(names: &[&str]) -> Vec<EntryKey> {
        names
            .iter()
            .map(|name| EntryKey::path(std::path::PathBuf::from(name), EntryKind::File))
            .collect()
    }

    #[test]
    fn sort_keeps_the_selection_on_the_same_entry() {
        // B-T07: 並べ替えても surviving key の選択を保持する。
        let before = keys(&["a", "b", "c", "d"]);
        let mut state = Selection::default();
        state.apply(pick(1, false, false), before.len());
        state.apply(SelectionIntent::Checkbox { index: 3 }, before.len());
        assert_eq!(state.selected, BTreeSet::from([1, 3]));

        let snapshot = state.snapshot(&before);
        // 並びが逆になる。"b" は index 2、"d" は index 0 へ移る。
        let after = keys(&["d", "c", "b", "a"]);
        state.restore(snapshot, &after);
        assert_eq!(state.selected, BTreeSet::from([0, 2]));
        // focus は最後に触った "d" を指したまま。
        assert_eq!(state.focused(), Some(0));
    }

    #[test]
    fn sort_does_not_move_a_vanished_selection_to_a_neighbour() {
        // B-T07: 消えた項目の選択を隣の行へ移さない。
        let before = keys(&["a", "b", "c"]);
        let mut state = Selection::default();
        state.apply(pick(1, false, false), before.len());

        let snapshot = state.snapshot(&before);
        let after = keys(&["a", "c"]);
        state.restore(snapshot, &after);
        assert_eq!(state.selected, BTreeSet::new());
        assert_eq!(state.focused(), None);
    }

    #[test]
    fn sort_commits_a_pending_range_before_remapping() {
        // B-T07: pending XOR を確定した状態で退避する。
        let before = keys(&["a", "b", "c", "d"]);
        let mut state = Selection::default();
        state.apply(pick(0, false, false), before.len());
        state.apply(pick(2, false, true), before.len());
        assert_eq!(state.len(), 3);

        let snapshot = state.snapshot(&before);
        let after = keys(&["d", "c", "b", "a"]);
        state.restore(snapshot, &after);
        // "a" と "b" と "c" が選択されたまま。並びは d,c,b,a。
        assert_eq!(state.selected, BTreeSet::from([1, 2, 3]));
        assert_eq!(state.len(), 3);
    }

    #[test]
    fn sort_clears_the_shift_session() {
        // 並びをまたぐ Shift session は意味を保てないので終了する。
        let before = keys(&["a", "b", "c", "d"]);
        let mut state = Selection::default();
        state.apply(pick(0, false, false), before.len());
        state.apply(move_focus(2, true), before.len());
        assert!(state.shift_anchor.is_some());

        let snapshot = state.snapshot(&before);
        let after = keys(&["d", "c", "b", "a"]);
        state.restore(snapshot, &after);
        assert!(state.shift_anchor.is_none());
        assert!(state.preview.is_empty());
    }

    #[test]
    fn ambiguous_keys_do_not_move_the_selection() {
        // 同じ key が複数ある並びでは、どれを指すか決められない。
        let before = keys(&["a", "b"]);
        let mut state = Selection::default();
        state.apply(pick(0, false, false), before.len());

        let snapshot = state.snapshot(&before);
        let after = keys(&["a", "a"]);
        state.restore(snapshot, &after);
        assert_eq!(state.selected, BTreeSet::new());
    }

    #[test]
    fn a_snapshot_can_be_kept_and_restored_later() {
        // 移動履歴の bookmark は snapshot を clone して持ち、戻った先で使う。
        let before = keys(&["a", "b", "c"]);
        let mut state = Selection::default();
        state.apply(pick(2, false, false), before.len());
        let focused = state.focused();
        let bookmark = state.snapshot(&before);

        // 一度別の並びへ移ってから、元の並びへ戻す。
        let after = keys(&["c", "b", "a"]);
        state.restore(bookmark.clone(), &after);
        assert_eq!(state.selected, BTreeSet::from([0]));
        assert_eq!(state.focused(), Some(0));

        state.restore(bookmark, &before);
        assert_eq!(state.selected, BTreeSet::from([2]));
        assert_eq!(state.focused(), focused);
    }

    #[test]
    fn rename_can_remap_a_snapshot_to_the_reconciled_key() {
        let before = keys(&["before.txt", "other.txt"]);
        let after = keys(&["other.txt", "after.txt"]);
        let mut state = Selection::default();
        state.apply(pick(0, false, false), before.len());

        let mut snapshot = state.snapshot(&before);
        snapshot.remap_key(&before[0], &after[1]);
        state.restore(snapshot, &after);

        assert_eq!(state.selected, BTreeSet::from([1]));
        assert_eq!(state.focused(), Some(1));
    }
}
