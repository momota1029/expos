//! 移動履歴。GPUI にも filesystem にも依存しない。
//!
//! location の唯一の owner はこの型で、[`crate::directory_session::DirectorySession`] が
//! 値として持つ。履歴は**訪問を試みた location** の記録なので、読取の成否では動かさない。
//! 読取成功後だけ cursor を動かす設計にすると、連続 Back / Forward が同じ古い cursor を
//! 参照し、並行読取の成功順に UI が動いてしまう。
//!
//! bookmark は item key と行内 offset、そのときの選択とフォーカスの best-effort 値。
//! hover、Shift session、画像は履歴に保存しない。

use crate::entry_identity::EntryKey;
use crate::location::Location;
use crate::selection::SelectionSnapshot;

/// 戻ってきたときに再現する表示状態。消えた項目は無視してよい。
#[derive(Clone, Debug, PartialEq)]
pub struct ViewBookmark {
    /// 先頭に見えている項目。
    pub key: EntryKey,
    /// その行の先頭からの offset（logical px）。
    pub offset: f32,
    /// そのときの選択とフォーカス。
    pub selection: SelectionSnapshot,
}

/// 一度訪れた場所。
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub location: Location,
    pub bookmark: Option<ViewBookmark>,
}

/// 訪問順の列と、今どの entry を見ているか。
#[derive(Default)]
pub struct NavigationHistory {
    entries: Vec<HistoryEntry>,
    cursor: usize,
}

impl NavigationHistory {
    /// 一 tab の上限。超えたら古い先頭を落とす。
    pub const LIMIT: usize = 100;

    /// Export only navigation values; runtime scan and watcher identities never cross this boundary.
    pub fn snapshot(&self) -> NavigationHistorySnapshot {
        NavigationHistorySnapshot {
            entries: self.entries.clone(),
            cursor: self.cursor,
        }
    }

    /// Borrow history values for bounded close-time preflight before allocating a snapshot.
    pub fn entries_ref(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Restore an already validated value snapshot without normalizing its cursor or entries.
    pub fn from_snapshot(snapshot: NavigationHistorySnapshot) -> Self {
        Self {
            entries: snapshot.entries,
            cursor: snapshot.cursor,
        }
    }

    /// 履歴の件数。テストと、将来の UI の disabled 判定が読む。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 今の位置。同じくテストと将来の UI が読む。
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 今見ている entry。
    ///
    /// `cursor` は常に範囲内に保つ。ここがずれると `location()` が消え、
    /// scan の commit も refresh も通らなくなる。念のため最後の entry へ倒す。
    pub fn current(&self) -> Option<&HistoryEntry> {
        self.entries
            .get(self.cursor)
            .or_else(|| self.entries.last())
    }

    pub fn location(&self) -> Option<&Location> {
        self.current().map(|entry| &entry.location)
    }

    /// 今の entry に保存されている表示状態。
    pub fn bookmark(&self) -> Option<&ViewBookmark> {
        self.current().and_then(|entry| entry.bookmark.as_ref())
    }

    /// 移動する。同じ location なら何もせず false を返す。
    ///
    /// 同じ場所の再読込は Refresh intent の役目なので、履歴は増やさない。
    /// また、分岐したときは cursor より先を捨てる。
    ///
    /// `bookmark_for_current` は**移動の直前に**渡す。移動と別の呼出しにすると、
    /// 遅れて届いた書込みが別の entry を指してしまう。
    pub fn navigate(
        &mut self,
        location: impl Into<Location>,
        bookmark_for_current: Option<ViewBookmark>,
    ) -> bool {
        // no-op の判定は最初に行う。truncate を先にすると、今の場所を再指定しただけで
        // 進む側の履歴が消える。
        let location = location.into();
        if self.location() == Some(&location) {
            return false;
        }
        self.set_bookmark(bookmark_for_current);
        self.entries.truncate(self.cursor + 1);
        self.entries.push(HistoryEntry {
            location,
            bookmark: None,
        });
        self.cursor = self.entries.len() - 1;
        self.trim();
        true
    }

    /// 戻れる履歴があるか。UI の disabled 判定が読む。
    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }

    /// 進める履歴があるか。
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }

    /// 戻る。範囲外なら何もしない。
    pub fn back(&mut self, bookmark_for_current: Option<ViewBookmark>) -> Option<Location> {
        if !self.can_back() {
            return None;
        }
        self.set_bookmark(bookmark_for_current);
        self.cursor -= 1;
        self.location().cloned()
    }

    /// 進む。範囲外なら何もしない。
    pub fn forward(&mut self, bookmark_for_current: Option<ViewBookmark>) -> Option<Location> {
        if !self.can_forward() {
            return None;
        }
        self.set_bookmark(bookmark_for_current);
        self.cursor += 1;
        self.location().cloned()
    }

    /// 履歴を作らずに、今の entry の表示状態を上書きする。
    pub fn set_bookmark(&mut self, bookmark: Option<ViewBookmark>) {
        if let Some(entry) = self.entries.get_mut(self.cursor) {
            entry.bookmark = bookmark;
        }
    }

    /// 上限を超えた分を古い側から落とす。
    ///
    /// `navigate` は必ず cursor を末尾へ置くので、落とすのは末尾にいるときだけ。
    fn trim(&mut self) {
        while self.entries.len() > Self::LIMIT {
            self.entries.remove(0);
            self.cursor = self.cursor.saturating_sub(1);
        }
        if !self.entries.is_empty() {
            // 上限の変更後も不変条件を保つ。
            self.cursor = self.cursor.min(self.entries.len() - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::EntryKind;
    use std::path::PathBuf;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn location(name: &str) -> Location {
        Location::Path(path(name))
    }

    /// 選択の中身は履歴の関心ではないので、空の snapshot で代表させる。
    fn bookmark(name: &str) -> ViewBookmark {
        ViewBookmark {
            key: EntryKey::path(path(name), EntryKind::File),
            offset: 12.0,
            selection: SelectionSnapshot::default(),
        }
    }

    /// B-T01 / C-T01 の履歴部分。A → B → C、Back、D へ移動。
    #[test]
    fn a_branch_drops_the_forward_entries() {
        let mut history = NavigationHistory::default();
        history.navigate(path("A"), None);
        history.navigate(path("B"), None);
        history.navigate(path("C"), None);
        assert_eq!(history.len(), 3);
        assert_eq!(history.cursor(), 2);

        assert_eq!(history.back(None), Some(location("B")));
        assert_eq!(history.cursor(), 1);

        // 分岐したので C は捨てられ、進む先は無い。
        assert!(history.navigate(path("D"), None));
        assert_eq!(history.len(), 3);
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| entry.location.clone())
                .collect::<Vec<_>>(),
            vec![location("A"), location("B"), location("D")]
        );
        assert_eq!(history.cursor(), 2);
        assert_eq!(history.forward(None), None);
    }

    /// 同じ場所の再指定は履歴を増やさず、進む側も壊さない。
    #[test]
    fn navigating_to_the_current_location_is_a_no_op() {
        let mut history = NavigationHistory::default();
        history.navigate(path("A"), None);
        history.navigate(path("B"), None);
        history.back(None);

        // 今の場所をもう一度指定しても、進む側の B は残る。
        assert!(!history.navigate(path("A"), None));
        assert_eq!(history.len(), 2);
        assert_eq!(history.cursor(), 0);
        assert_eq!(history.forward(None), Some(location("B")));
    }

    #[test]
    fn back_and_forward_stop_at_the_ends() {
        let mut history = NavigationHistory::default();
        assert_eq!(history.back(None), None);
        assert_eq!(history.forward(None), None);

        history.navigate(path("A"), None);
        assert_eq!(history.back(None), None);
        assert_eq!(history.forward(None), None);
        assert_eq!(history.location(), Some(&location("A")));
    }

    /// 空の履歴でも最初の一件は入る。demo は location を持たない。
    #[test]
    fn the_first_entry_needs_no_current_entry() {
        let mut history = NavigationHistory::default();
        assert!(history.is_empty());
        assert!(history.navigate(path("A"), Some(bookmark("x"))));
        assert_eq!(history.len(), 1);
        assert_eq!(history.cursor(), 0);
        // 置き場所が無いので bookmark は捨てる。
        assert_eq!(history.bookmark(), None);
    }

    /// bookmark は「移動直前の entry」に付く。cursor の移動と同時でないと、
    /// 遅れて届いた書込みが別の entry を指す。
    #[test]
    fn a_bookmark_is_stored_on_the_entry_that_was_left() {
        let mut history = NavigationHistory::default();
        history.navigate(path("A"), None);
        history.navigate(path("B"), None);
        // B から戻るときに、B のスクロール位置を渡す。
        history.back(Some(bookmark("in-b")));
        assert_eq!(history.location(), Some(&location("A")));
        // A に付いてはいけない。
        assert_eq!(history.bookmark(), None);

        history.forward(Some(bookmark("in-a")));
        assert_eq!(history.location(), Some(&location("B")));
        assert_eq!(history.bookmark(), Some(&bookmark("in-b")));
    }

    #[test]
    fn the_oldest_entry_is_dropped_past_the_limit() {
        let mut history = NavigationHistory::default();
        for index in 0..NavigationHistory::LIMIT {
            history.navigate(path(&format!("p{index}")), None);
        }
        assert_eq!(history.len(), NavigationHistory::LIMIT);
        assert_eq!(history.cursor(), NavigationHistory::LIMIT - 1);

        history.navigate(path("last"), None);
        assert_eq!(history.len(), NavigationHistory::LIMIT);
        // 先頭が落ちたので cursor も詰める。
        assert_eq!(history.cursor(), NavigationHistory::LIMIT - 1);
        assert_eq!(history.location(), Some(&location("last")));

        // 古い先頭は消え、直近の分だけ戻れる。
        let mut steps = 0;
        while history.back(None).is_some() {
            steps += 1;
        }
        assert_eq!(steps, NavigationHistory::LIMIT - 1);
        assert_eq!(history.location(), Some(&location("p1")));
    }

    /// C-T03 の履歴部分。履歴を作らない操作で件数が動かない。
    #[test]
    fn bookmark_updates_do_not_change_the_history() {
        let mut history = NavigationHistory::default();
        history.navigate(path("A"), None);
        history.navigate(path("B"), None);
        let (len, cursor) = (history.len(), history.cursor());

        history.set_bookmark(Some(bookmark("b")));
        assert_eq!(history.len(), len);
        assert_eq!(history.cursor(), cursor);
        assert_eq!(history.bookmark(), Some(&bookmark("b")));
    }

    /// 同じ場所が離れて二度出ても、cursor で区別できる。
    #[test]
    fn a_repeated_location_gets_its_own_entry() {
        let mut history = NavigationHistory::default();
        history.navigate(path("A"), None);
        history.navigate(path("B"), None);
        history.navigate(path("A"), None);
        assert_eq!(history.len(), 3);
        assert_eq!(history.cursor(), 2);
        // 今は A。戻ると B。
        assert_eq!(history.back(None), Some(location("B")));
        assert_eq!(history.back(None), Some(location("A")));
        assert_eq!(history.back(None), None);
    }

    #[test]
    fn path_pc_path_back_and_forward_keep_the_exact_locations() {
        let mut history = NavigationHistory::default();
        history.navigate(path("A"), None);
        history.navigate(Location::ThisPc, None);
        history.navigate(path("B"), None);
        assert_eq!(history.back(None), Some(Location::ThisPc));
        assert_eq!(history.back(None), Some(location("A")));
        assert_eq!(history.forward(None), Some(Location::ThisPc));
        assert_eq!(history.forward(None), Some(location("B")));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NavigationHistorySnapshot {
    pub entries: Vec<HistoryEntry>,
    pub cursor: usize,
}
