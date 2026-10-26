//! 一覧の並べ替え規則。GPUI にも filesystem にも依存しない。
//!
//! v5 は並べ替えのたびにディレクトリを読み直していたが、v6 の `Entry` は
//! size / modified を raw fact として持っているので、手元の並びを入れ替えるだけで
//! 同じ見た目になる。再列挙はしない。

use crate::directory::{Entry, EntryKind};
use std::cmp::Ordering;

/// 並べ替えに使う列。v5 の `ColumnKind` と同じ4種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Name,
    Kind,
    Size,
    Modified,
}

/// 一覧の並べ替え状態。`None` は v5 と同じくフォルダ優先 + 名前順。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortState {
    #[default]
    None,
    Asc(Column),
    Desc(Column),
}

impl SortState {
    /// 列ヘッダのクリックで次に移る状態。v5 の `RowsState::toggle_sort` と同じ巡回。
    /// None -> Asc -> Desc -> None、別の列を押したときはその列の Asc へ。
    pub fn toggled(self, column: Column) -> Self {
        match self {
            Self::None => Self::Asc(column),
            Self::Asc(current) if current == column => Self::Desc(column),
            Self::Desc(current) if current == column => Self::None,
            Self::Asc(_) | Self::Desc(_) => Self::Asc(column),
        }
    }

    /// この列が今の並べ替え対象か。ヘッダの表示に使う。
    pub fn column(self) -> Option<Column> {
        match self {
            Self::None => None,
            Self::Asc(column) | Self::Desc(column) => Some(column),
        }
    }

    pub fn is_descending(self) -> bool {
        matches!(self, Self::Desc(_))
    }
}

/// 一覧を並べ替える。
///
/// フォルダ優先は列を指定したときも崩さない。v5 は `SortState::None` のときだけ
/// フォルダを先に置いていたが、列を指定するとフォルダとファイルが混ざっていた。
/// エクスプローラー寄りの見え方に合わせ、どの列でもフォルダを先にまとめる。
pub fn sort_entries(entries: &mut [Entry], sort: SortState) {
    match sort {
        SortState::None => entries.sort_by(|a, b| {
            // 既定はフォルダ優先 + 名前順。
            folders_first(a, b).then_with(|| compare_name(a, b))
        }),
        SortState::Asc(column) => entries
            .sort_by(|a, b| folders_first(a, b).then_with(|| compare_by_column(a, b, column))),
        SortState::Desc(column) => {
            // 降順でも「フォルダ優先」と「未取得は後ろ」は崩さない。
            // v5 の `cmp_option_*` は値の比較と None の位置決めを分けており、
            // `reverse()` を丸ごと掛けると None が先頭へ来てしまう。
            entries.sort_by(|a, b| {
                folders_first(a, b).then_with(|| compare_by_column_desc(a, b, column))
            })
        }
    }
}

/// フォルダをファイルより先に置く。
fn folders_first(a: &Entry, b: &Entry) -> Ordering {
    is_folder(b).cmp(&is_folder(a))
}

fn is_folder(entry: &Entry) -> bool {
    matches!(entry.kind, EntryKind::Folder)
}

/// 列ごとの比較。同値なら名前で安定させる(v5 の `cmp_by_column` と同じ)。
fn compare_by_column(a: &Entry, b: &Entry, column: Column) -> Ordering {
    match column {
        Column::Name => compare_name(a, b),
        Column::Kind => compare_text(&kind_key(a), &kind_key(b)).then_with(|| compare_name(a, b)),
        Column::Size => {
            compare_optional(size_for_sort(a), size_for_sort(b)).then_with(|| compare_name(a, b))
        }
        Column::Modified => {
            compare_optional(a.modified, b.modified).then_with(|| compare_name(a, b))
        }
    }
}

/// 降順の比較。値の順序だけを反転し、`None` は後ろに残す。
fn compare_by_column_desc(a: &Entry, b: &Entry, column: Column) -> Ordering {
    match column {
        Column::Name => compare_name(a, b).reverse(),
        Column::Kind => compare_text(&kind_key(a), &kind_key(b))
            .reverse()
            .then_with(|| compare_name(a, b)),
        Column::Size => compare_optional_desc(size_for_sort(a), size_for_sort(b))
            .then_with(|| compare_name(a, b)),
        Column::Modified => {
            compare_optional_desc(a.modified, b.modified).then_with(|| compare_name(a, b))
        }
    }
}

/// folder の外部 index size は exact file size と区別して保持するが、Size sort では
/// 明示的に利用する。folder-first の規則はこの前段で維持される。
fn size_for_sort(entry: &Entry) -> Option<u64> {
    entry.size.or(entry.folder_size.map(|fact| fact.bytes))
}

/// 種類の比較に使うキー。v5 は列挙時に小文字の拡張子を持っていたので、
/// ここでも表示用の `kind_label()` ではなく小文字のキーを作る。
fn kind_key(entry: &Entry) -> String {
    match entry.kind {
        EntryKind::Folder => "folder".to_string(),
        EntryKind::File => extension(entry).unwrap_or_else(|| "file".to_string()),
    }
}

fn extension(entry: &Entry) -> Option<String> {
    std::path::Path::new(&entry.name)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
}

/// 名前の自然順比較。数字は数値として扱い、大文字小文字は無視する。
/// v5 の `sort-spec` crate と同じ規則を、必要な分だけここへ持ってくる。
fn compare_name(a: &Entry, b: &Entry) -> Ordering {
    natural_cmp(&a.name, &b.name)
}

fn compare_text(a: &str, b: &str) -> Ordering {
    a.cmp(b)
}

/// `None` は常に後ろへ置く。v5 の `cmp_option_u64` / `cmp_option_time` と同じ。
fn compare_optional<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// 降順版。値だけを反転し、`None` はやはり後ろに置く。
fn compare_optional_desc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[derive(Debug)]
enum Token<'a> {
    Number(&'a str),
    Text(&'a str),
}

fn tokenize(input: &str) -> TokenizeIter<'_> {
    let mut after = input.chars();
    let first = after.next();
    TokenizeIter {
        begin: input,
        left: after.as_str(),
        in_number: first.is_some_and(|c| c.is_ascii_digit()),
        is_end: input.is_empty(),
    }
}

struct TokenizeIter<'a> {
    begin: &'a str,
    left: &'a str,
    in_number: bool,
    is_end: bool,
}

impl<'a> Iterator for TokenizeIter<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_end {
            return None;
        }

        let word_len = self.begin.len() - self.left.len();
        let mut iter = self.left.chars();
        let first = iter.next();

        if self.in_number {
            match first {
                None => {
                    self.is_end = true;
                    Some(Token::Number(&self.begin[0..word_len]))
                }
                Some(c) if c.is_ascii_digit() => {
                    self.left = iter.as_str();
                    self.next()
                }
                Some(_) => {
                    let token = Token::Number(&self.begin[0..word_len]);
                    self.begin = self.left;
                    self.left = iter.as_str();
                    self.in_number = false;
                    Some(token)
                }
            }
        } else {
            match first {
                None => {
                    self.is_end = true;
                    Some(Token::Text(&self.begin[0..word_len]))
                }
                Some(c) if c.is_ascii_digit() => {
                    let token = Token::Text(&self.begin[0..word_len]);
                    self.begin = self.left;
                    self.left = iter.as_str();
                    self.in_number = true;
                    Some(token)
                }
                Some(_) => {
                    self.left = iter.as_str();
                    self.next()
                }
            }
        }
    }
}

/// 自然順比較。数字の並びは桁数と値で比べ、先頭の 0 は無視する。
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a_tokens = tokenize(a);
    let mut b_tokens = tokenize(b);

    loop {
        let cmp = match (a_tokens.next(), b_tokens.next()) {
            (Some(Token::Text(a_text)), Some(Token::Text(b_text))) => {
                compare_text_case_insensitive(a_text, b_text)
            }
            (Some(Token::Number(a_num)), Some(Token::Number(b_num))) => {
                compare_number_tokens(a_num, b_num)
            }
            (Some(Token::Number(_)), Some(Token::Text(_))) => Ordering::Less,
            (Some(Token::Text(_)), Some(Token::Number(_))) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (None, None) => return Ordering::Equal,
        };
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
}

fn compare_number_tokens(a: &str, b: &str) -> Ordering {
    let a_trim = a.trim_start_matches('0');
    let b_trim = b.trim_start_matches('0');
    let a_norm = if a_trim.is_empty() { "0" } else { a_trim };
    let b_norm = if b_trim.is_empty() { "0" } else { b_trim };

    let len_cmp = a_norm.len().cmp(&b_norm.len());
    if len_cmp != Ordering::Equal {
        return len_cmp;
    }
    let digit_cmp = a_norm.cmp(b_norm);
    if digit_cmp != Ordering::Equal {
        return digit_cmp;
    }
    // 数値が同値なら桁数の短い方を先にする("2" < "02")。
    a.len().cmp(&b.len())
}

fn compare_text_case_insensitive(a: &str, b: &str) -> Ordering {
    for (a_byte, b_byte) in a.bytes().zip(b.bytes()) {
        let a_low = a_byte.to_ascii_lowercase();
        let b_low = b_byte.to_ascii_lowercase();
        if a_low != b_low {
            return a_low.cmp(&b_low);
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn entry(name: &str, kind: EntryKind, size: Option<u64>, modified_secs: Option<u64>) -> Entry {
        Entry {
            is_hidden: false,
            // 並べ替えのテストでは名前をそのまま key の代わりにする。
            key: crate::entry_identity::EntryKey::path(std::path::PathBuf::from(name), kind),
            name: name.to_string(),
            path: None,
            kind,
            link: None,
            link_target: None,
            source_identity: None,
            size,
            folder_size: None,
            modified: modified_secs.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }

    fn file(name: &str) -> Entry {
        entry(name, EntryKind::File, Some(0), Some(0))
    }

    fn folder(name: &str) -> Entry {
        entry(name, EntryKind::Folder, None, Some(0))
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn toggle_cycles_through_three_states_per_column() {
        let sort = SortState::None;
        let sort = sort.toggled(Column::Size);
        assert_eq!(sort, SortState::Asc(Column::Size));
        let sort = sort.toggled(Column::Size);
        assert_eq!(sort, SortState::Desc(Column::Size));
        let sort = sort.toggled(Column::Size);
        assert_eq!(sort, SortState::None);
        // 別の列を押すと、その列の昇順から始まる。
        assert_eq!(
            SortState::Desc(Column::Size).toggled(Column::Name),
            SortState::Asc(Column::Name)
        );
    }

    #[test]
    fn default_order_puts_folders_first_and_sorts_naturally() {
        let mut entries = vec![file("file10"), folder("b"), file("file2"), folder("a")];
        sort_entries(&mut entries, SortState::None);
        assert_eq!(names(&entries), ["a", "b", "file2", "file10"]);
    }

    #[test]
    fn name_sort_is_natural_and_case_insensitive() {
        let mut entries = vec![file("FileB"), file("filea"), file("file10"), file("file2")];
        sort_entries(&mut entries, SortState::Asc(Column::Name));
        // 数字トークンは文字トークンより前に来るので、file2/file10 が filea より先。
        assert_eq!(names(&entries), ["file2", "file10", "filea", "FileB"]);
        sort_entries(&mut entries, SortState::Desc(Column::Name));
        assert_eq!(names(&entries), ["FileB", "filea", "file10", "file2"]);
    }

    #[test]
    fn folders_stay_first_in_every_column_and_direction() {
        let mut entries = vec![
            entry("z.txt", EntryKind::File, Some(1), Some(1)),
            entry("a", EntryKind::Folder, None, Some(9)),
            entry("m.txt", EntryKind::File, Some(5), Some(5)),
            entry("b", EntryKind::Folder, None, Some(2)),
        ];
        for sort in [
            SortState::None,
            SortState::Asc(Column::Name),
            SortState::Desc(Column::Name),
            SortState::Asc(Column::Size),
            SortState::Desc(Column::Size),
            SortState::Asc(Column::Modified),
            SortState::Desc(Column::Modified),
            SortState::Asc(Column::Kind),
            SortState::Desc(Column::Kind),
        ] {
            sort_entries(&mut entries, sort);
            assert!(
                matches!(entries[0].kind, EntryKind::Folder)
                    && matches!(entries[1].kind, EntryKind::Folder),
                "{sort:?} put a file first: {:?}",
                names(&entries)
            );
        }
    }

    #[test]
    fn size_sort_keeps_unknown_sizes_last_in_both_directions() {
        let mut entries = vec![
            entry("b", EntryKind::File, Some(20), Some(0)),
            entry("a", EntryKind::File, Some(10), Some(0)),
            entry("folder", EntryKind::Folder, None, Some(0)),
        ];
        sort_entries(&mut entries, SortState::Asc(Column::Size));
        // フォルダは先頭にまとまり、その中ではサイズ未取得なので名前順。
        assert_eq!(names(&entries), ["folder", "a", "b"]);
        // 降順でもフォルダ優先と未取得の位置は変わらない。
        sort_entries(&mut entries, SortState::Desc(Column::Size));
        assert_eq!(names(&entries), ["folder", "b", "a"]);
    }

    #[test]
    fn unknown_sizes_stay_last_among_files() {
        let mut entries = vec![
            entry("b", EntryKind::File, Some(20), Some(0)),
            entry("unknown", EntryKind::File, None, Some(0)),
            entry("a", EntryKind::File, Some(10), Some(0)),
        ];
        sort_entries(&mut entries, SortState::Asc(Column::Size));
        assert_eq!(names(&entries), ["a", "b", "unknown"]);
        sort_entries(&mut entries, SortState::Desc(Column::Size));
        assert_eq!(names(&entries), ["b", "a", "unknown"]);
    }

    #[test]
    fn modified_sort_uses_the_timestamp() {
        let mut entries = vec![
            entry("new", EntryKind::File, Some(0), Some(200)),
            entry("old", EntryKind::File, Some(0), Some(100)),
            entry("unknown", EntryKind::File, Some(0), None),
        ];
        sort_entries(&mut entries, SortState::Asc(Column::Modified));
        assert_eq!(names(&entries), ["old", "new", "unknown"]);
        sort_entries(&mut entries, SortState::Desc(Column::Modified));
        assert_eq!(names(&entries), ["new", "old", "unknown"]);
    }

    #[test]
    fn kind_sort_uses_the_lowercase_extension_and_breaks_ties_by_name() {
        let mut entries = vec![
            file("b.txt"),
            file("a.txt"),
            folder("folder"),
            file("image.png"),
            file("noext"),
        ];
        sort_entries(&mut entries, SortState::Asc(Column::Kind));
        // フォルダが先頭にまとまり、ファイルは file < noext < png < txt。
        // 同種は名前順で安定する。
        assert_eq!(
            names(&entries),
            ["folder", "noext", "image.png", "a.txt", "b.txt"]
        );
    }

    #[test]
    fn equal_keys_fall_back_to_the_name() {
        let mut entries = vec![
            entry("c", EntryKind::File, Some(5), Some(1)),
            entry("a", EntryKind::File, Some(5), Some(1)),
            entry("b", EntryKind::File, Some(5), Some(1)),
        ];
        sort_entries(&mut entries, SortState::Asc(Column::Size));
        assert_eq!(names(&entries), ["a", "b", "c"]);
    }
}
