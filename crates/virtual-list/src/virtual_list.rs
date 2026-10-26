//! Data structures for variable-height virtual lists.
//!
//! This crate deliberately does not depend on GPUI. It owns only the mapping
//! between row indices and logical heights; rendering and measurement remain
//! at the composition boundary.

use std::ops::Range;

/// The content and trailing gap for one list row, in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RowHeight {
    pub content: f32,
    pub trailing: f32,
}

impl RowHeight {
    pub const ZERO: Self = Self {
        content: 0.0,
        trailing: 0.0,
    };

    pub fn total(self) -> f32 {
        self.content + self.trailing
    }

    fn sanitized(self) -> Self {
        Self {
            content: self.content.max(0.0),
            trailing: self.trailing.max(0.0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Affine {
    multiplier: f32,
    addend: f32,
}

impl Affine {
    const IDENTITY: Self = Self {
        multiplier: 1.0,
        addend: 0.0,
    };

    fn constant(value: f32) -> Self {
        Self {
            multiplier: 0.0,
            addend: value,
        }
    }

    fn scale(multiplier: f32) -> Self {
        Self {
            multiplier,
            addend: 0.0,
        }
    }

    /// Applies `older` first, then `self`.
    fn after(self, older: Self) -> Self {
        Self {
            multiplier: self.multiplier * older.multiplier,
            addend: self.multiplier * older.addend + self.addend,
        }
    }

    fn is_identity(self) -> bool {
        self.multiplier == 1.0 && self.addend == 0.0
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Summary {
    content: f32,
    trailing: f32,
    rows: usize,
}

impl Summary {
    fn row(height: RowHeight) -> Self {
        Self {
            content: height.content,
            trailing: height.trailing,
            rows: 1,
        }
    }

    fn total(self) -> f32 {
        self.content + self.trailing
    }

    fn merge(left: Self, right: Self) -> Self {
        Self {
            content: left.content + right.content,
            trailing: left.trailing + right.trailing,
            rows: left.rows + right.rows,
        }
    }

    fn apply(self, transform: Affine) -> Self {
        Self {
            content: self.content * transform.multiplier + self.rows as f32 * transform.addend,
            ..self
        }
    }
}

/// A lazy segment tree over variable-height rows.
///
/// Content height is adjusted in ranges while trailing gaps stay fixed. This
/// lets a list refine estimated row heights without repeatedly walking a huge
/// directory.
#[derive(Debug)]
pub struct RowHeights {
    len: usize,
    leaf_count: usize,
    summaries: Vec<Summary>,
    pending: Vec<Affine>,
}

impl RowHeights {
    pub fn new(len: usize) -> Self {
        let leaf_count = len.max(1).next_power_of_two();
        let mut summaries = vec![Summary::default(); leaf_count * 2];
        for index in 0..len {
            summaries[leaf_count + index] = Summary::row(RowHeight::ZERO);
        }
        for node in (1..leaf_count).rev() {
            summaries[node] = Summary::merge(summaries[node * 2], summaries[node * 2 + 1]);
        }
        Self {
            len,
            leaf_count,
            summaries,
            pending: vec![Affine::IDENTITY; leaf_count * 2],
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn reset(&mut self, len: usize) {
        *self = Self::new(len);
    }

    pub fn total_height(&self) -> f32 {
        self.summaries[1].total()
    }

    /// The logical offset directly before `index`.
    pub fn offset_before(&self, index: usize) -> f32 {
        self.range_summary(0..index.min(self.len)).total()
    }

    pub fn row_height(&self, index: usize) -> Option<RowHeight> {
        (index < self.len).then(|| {
            let summary = self.range_summary(index..index + 1);
            RowHeight {
                content: summary.content,
                trailing: summary.trailing,
            }
        })
    }

    pub fn set(&mut self, index: usize, height: RowHeight) {
        if index >= self.len {
            return;
        }
        self.set_at(index, height.sanitized(), 1, 0, self.leaf_count);
    }

    /// Replaces only content heights in `range`; gaps remain theme-owned.
    pub fn set_content(&mut self, range: Range<usize>, content: f32) {
        self.apply_range(range, Affine::constant(content.max(0.0)));
    }

    /// Scales only content heights in `range`; gaps remain theme-owned.
    pub fn scale_content(&mut self, range: Range<usize>, multiplier: f32) {
        if !multiplier.is_finite() || multiplier < 0.0 {
            return;
        }
        self.apply_range(range, Affine::scale(multiplier));
    }

    /// Returns the row containing `offset`. `len()` means past the content.
    pub fn row_at_offset(&self, offset: f32) -> usize {
        if offset <= 0.0 || self.len == 0 {
            return 0;
        }
        if offset >= self.total_height() {
            return self.len;
        }

        let mut node = 1;
        let mut consumed = 0.0;
        let mut inherited = Affine::IDENTITY;
        while node < self.leaf_count {
            let child_transform = inherited.after(self.pending[node]);
            let left = self.summaries[node * 2].apply(child_transform);
            if consumed + left.total() <= offset {
                consumed += left.total();
                node = node * 2 + 1;
            } else {
                node *= 2;
            }
            inherited = child_transform;
        }
        (node - self.leaf_count).min(self.len)
    }

    /// Finds the rows needed to cover a viewport, with extra rows on either
    /// side to hide range changes while the user scrolls.
    pub fn visible_range(
        &self,
        offset: f32,
        viewport_height: f32,
        overscan: usize,
    ) -> Range<usize> {
        if self.is_empty() {
            return 0..0;
        }

        let start = self.row_at_offset(offset.max(0.0)).saturating_sub(overscan);
        let end_offset = (offset.max(0.0) + viewport_height.max(0.0)).min(self.total_height());
        let end = self
            .row_at_offset(end_offset)
            .saturating_add(1)
            .min(self.len);
        start..end.max(start)
    }

    fn apply_range(&mut self, range: Range<usize>, transform: Affine) {
        let start = range.start.min(self.len);
        let end = range.end.min(self.len);
        if start >= end {
            return;
        }
        self.apply_range_at(start, end, transform, 1, 0, self.leaf_count);
    }

    fn set_at(&mut self, index: usize, height: RowHeight, node: usize, start: usize, end: usize) {
        if end - start == 1 {
            self.summaries[node] = Summary::row(height);
            self.pending[node] = Affine::IDENTITY;
            return;
        }
        self.push(node);
        let middle = (start + end) / 2;
        if index < middle {
            self.set_at(index, height, node * 2, start, middle);
        } else {
            self.set_at(index, height, node * 2 + 1, middle, end);
        }
        self.pull(node);
    }

    fn apply_range_at(
        &mut self,
        query_start: usize,
        query_end: usize,
        transform: Affine,
        node: usize,
        start: usize,
        end: usize,
    ) {
        if query_end <= start || end <= query_start || self.summaries[node].rows == 0 {
            return;
        }
        if query_start <= start && end <= query_end {
            self.apply(node, transform);
            return;
        }
        self.push(node);
        let middle = (start + end) / 2;
        self.apply_range_at(query_start, query_end, transform, node * 2, start, middle);
        self.apply_range_at(query_start, query_end, transform, node * 2 + 1, middle, end);
        self.pull(node);
    }

    fn range_summary(&self, range: Range<usize>) -> Summary {
        self.range_summary_at(
            range.start.min(self.len),
            range.end.min(self.len),
            1,
            0,
            self.leaf_count,
            Affine::IDENTITY,
        )
    }

    fn range_summary_at(
        &self,
        query_start: usize,
        query_end: usize,
        node: usize,
        start: usize,
        end: usize,
        inherited: Affine,
    ) -> Summary {
        if query_end <= start || end <= query_start || self.summaries[node].rows == 0 {
            return Summary::default();
        }
        if query_start <= start && end <= query_end {
            return self.summaries[node].apply(inherited);
        }
        let inherited = inherited.after(self.pending[node]);
        let middle = (start + end) / 2;
        Summary::merge(
            self.range_summary_at(query_start, query_end, node * 2, start, middle, inherited),
            self.range_summary_at(query_start, query_end, node * 2 + 1, middle, end, inherited),
        )
    }

    fn apply(&mut self, node: usize, transform: Affine) {
        self.summaries[node] = self.summaries[node].apply(transform);
        self.pending[node] = transform.after(self.pending[node]);
    }

    fn push(&mut self, node: usize) {
        if node >= self.leaf_count || self.pending[node].is_identity() {
            return;
        }
        let pending = self.pending[node];
        self.apply(node * 2, pending);
        self.apply(node * 2 + 1, pending);
        self.pending[node] = Affine::IDENTITY;
    }

    fn pull(&mut self, node: usize) {
        self.summaries[node] =
            Summary::merge(self.summaries[node * 2], self.summaries[node * 2 + 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heights() -> RowHeights {
        let mut heights = RowHeights::new(4);
        heights.set(
            0,
            RowHeight {
                content: 10.0,
                trailing: 2.0,
            },
        );
        heights.set(
            1,
            RowHeight {
                content: 20.0,
                trailing: 3.0,
            },
        );
        heights.set(
            2,
            RowHeight {
                content: 30.0,
                trailing: 4.0,
            },
        );
        heights.set(
            3,
            RowHeight {
                content: 40.0,
                trailing: 0.0,
            },
        );
        heights
    }

    #[test]
    fn maps_offsets_to_rows_at_boundaries() {
        let heights = heights();
        assert_eq!(heights.total_height(), 109.0);
        assert_eq!(heights.offset_before(0), 0.0);
        assert_eq!(heights.offset_before(2), 35.0);
        assert_eq!(heights.row_at_offset(-1.0), 0);
        assert_eq!(heights.row_at_offset(0.0), 0);
        assert_eq!(heights.row_at_offset(11.9), 0);
        assert_eq!(heights.row_at_offset(12.0), 1);
        assert_eq!(heights.row_at_offset(34.9), 1);
        assert_eq!(heights.row_at_offset(35.0), 2);
        assert_eq!(heights.row_at_offset(109.0), 4);
    }

    #[test]
    fn finds_only_rows_covering_the_viewport() {
        let heights = heights();
        assert_eq!(heights.visible_range(34.0, 2.0, 0), 1..3);
        assert_eq!(heights.visible_range(35.0, 30.0, 1), 1..3);
        assert_eq!(heights.visible_range(200.0, 30.0, 1), 3..4);
        assert_eq!(RowHeights::new(0).visible_range(0.0, 30.0, 1), 0..0);
    }

    #[test]
    fn range_updates_preserve_trailing_gaps() {
        let mut heights = heights();
        heights.scale_content(1..3, 2.0);
        assert_eq!(
            heights.row_height(1),
            Some(RowHeight {
                content: 40.0,
                trailing: 3.0
            })
        );
        assert_eq!(
            heights.row_height(2),
            Some(RowHeight {
                content: 60.0,
                trailing: 4.0
            })
        );
        heights.set_content(0..2, 7.0);
        assert_eq!(
            heights.row_height(0),
            Some(RowHeight {
                content: 7.0,
                trailing: 2.0
            })
        );
        assert_eq!(
            heights.row_height(1),
            Some(RowHeight {
                content: 7.0,
                trailing: 3.0
            })
        );
        assert_eq!(heights.total_height(), 123.0);
    }

    #[test]
    fn point_updates_override_pending_estimates() {
        let mut heights = RowHeights::new(8);
        heights.set_content(0..8, 24.0);
        heights.scale_content(2..7, 1.5);
        heights.set(
            4,
            RowHeight {
                content: 31.0,
                trailing: 5.0,
            },
        );
        assert_eq!(
            heights.row_height(1),
            Some(RowHeight {
                content: 24.0,
                trailing: 0.0
            })
        );
        assert_eq!(
            heights.row_height(2),
            Some(RowHeight {
                content: 36.0,
                trailing: 0.0
            })
        );
        assert_eq!(
            heights.row_height(4),
            Some(RowHeight {
                content: 31.0,
                trailing: 5.0
            })
        );
        assert_eq!(heights.offset_before(5), 156.0);
    }

    #[test]
    fn empty_and_out_of_bounds_operations_are_safe() {
        let mut heights = RowHeights::new(0);
        heights.set(
            0,
            RowHeight {
                content: 1.0,
                trailing: 1.0,
            },
        );
        heights.scale_content(0..1, 2.0);
        assert_eq!(heights.total_height(), 0.0);
        assert_eq!(heights.row_at_offset(10.0), 0);
        heights.reset(1);
        heights.set(
            0,
            RowHeight {
                content: -1.0,
                trailing: f32::NAN,
            },
        );
        assert_eq!(heights.row_height(0), Some(RowHeight::ZERO));
    }

    #[test]
    fn lazy_updates_match_a_naive_large_directory_model() {
        let len = 16_384;
        let mut heights = RowHeights::new(len);
        let mut expected = vec![RowHeight::ZERO; len];

        for (index, height) in expected.iter_mut().enumerate() {
            *height = RowHeight {
                content: 18.0 + (index % 7) as f32,
                trailing: (index % 3) as f32,
            };
            heights.set(index, *height);
        }

        heights.scale_content(1_000..15_000, 1.25);
        for height in &mut expected[1_000..15_000] {
            height.content *= 1.25;
        }
        heights.set_content(4_000..12_000, 31.0);
        for height in &mut expected[4_000..12_000] {
            height.content = 31.0;
        }
        heights.set(
            8_192,
            RowHeight {
                content: 47.0,
                trailing: 5.0,
            },
        );
        expected[8_192] = RowHeight {
            content: 47.0,
            trailing: 5.0,
        };

        let mut offset = 0.0;
        for (index, height) in expected.iter().copied().enumerate() {
            assert_eq!(heights.offset_before(index), offset);
            assert_eq!(heights.row_height(index), Some(height));
            assert_eq!(heights.row_at_offset(offset), index);
            offset += height.total();
        }
        assert_eq!(heights.total_height(), offset);
        assert_eq!(heights.row_at_offset(offset), len);
    }
}
