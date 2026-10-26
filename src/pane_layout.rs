use crate::session_store::{PaneLayoutSnapshot, SplitDirectionValue};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PaneId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PaneLayoutNode {
    Pane(PaneId),
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl PaneLayoutNode {
    pub(crate) fn single(pane: PaneId) -> Self {
        Self::Pane(pane)
    }

    pub(crate) fn from_snapshot(snapshot: &PaneLayoutSnapshot) -> Self {
        match snapshot {
            PaneLayoutSnapshot::Pane { key } => Self::Pane(PaneId(*key)),
            PaneLayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => Self::Split {
                direction: match direction {
                    SplitDirectionValue::Horizontal => SplitDirection::Horizontal,
                    SplitDirectionValue::Vertical => SplitDirection::Vertical,
                },
                ratio: *ratio,
                first: Box::new(Self::from_snapshot(first)),
                second: Box::new(Self::from_snapshot(second)),
            },
        }
    }

    pub(crate) fn to_snapshot(&self) -> PaneLayoutSnapshot {
        match self {
            Self::Pane(PaneId(key)) => PaneLayoutSnapshot::Pane { key: *key },
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => PaneLayoutSnapshot::Split {
                direction: match direction {
                    SplitDirection::Horizontal => SplitDirectionValue::Horizontal,
                    SplitDirection::Vertical => SplitDirectionValue::Vertical,
                },
                ratio: *ratio,
                first: Box::new(first.to_snapshot()),
                second: Box::new(second.to_snapshot()),
            },
        }
    }

    pub(crate) fn pane_ids(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        self.collect_pane_ids(&mut panes);
        panes
    }

    /// Produce the directory-only tree used by session snapshots. Transient
    /// leaves are omitted and unary splits collapse without changing the
    /// geometry of the surviving branch.
    pub(crate) fn retaining(&self, retained: &std::collections::HashSet<PaneId>) -> Option<Self> {
        match self {
            Self::Pane(id) => retained.contains(id).then_some(Self::Pane(*id)),
            Self::Split { direction, ratio, first, second } => {
                match (first.retaining(retained), second.retaining(retained)) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        direction: *direction,
                        ratio: *ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    fn collect_pane_ids(&self, panes: &mut Vec<PaneId>) {
        match self {
            Self::Pane(id) => panes.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_pane_ids(panes);
                second.collect_pane_ids(panes);
            }
        }
    }

    pub(crate) fn contains(&self, pane_id: PaneId) -> bool {
        match self {
            Self::Pane(id) => *id == pane_id,
            Self::Split { first, second, .. } => {
                first.contains(pane_id) || second.contains(pane_id)
            }
        }
    }

    pub(crate) fn first_pane(&self) -> PaneId {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }

    fn last_pane(&self) -> PaneId {
        match self {
            Self::Pane(id) => *id,
            Self::Split { second, .. } => second.last_pane(),
        }
    }

    pub(crate) fn width_fractions(&self) -> Vec<(PaneId, f32)> {
        let mut panes = Vec::new();
        self.collect_width_fractions(1.0, &mut panes);
        panes
    }

    fn collect_width_fractions(&self, width: f32, panes: &mut Vec<(PaneId, f32)>) {
        match self {
            Self::Pane(id) => panes.push((*id, width)),
            Self::Split {
                direction: SplitDirection::Horizontal,
                ratio,
                first,
                second,
            } => {
                first.collect_width_fractions(width * ratio, panes);
                second.collect_width_fractions(width * (1.0 - ratio), panes);
            }
            Self::Split {
                direction: SplitDirection::Vertical,
                first,
                second,
                ..
            } => {
                first.collect_width_fractions(width, panes);
                second.collect_width_fractions(width, panes);
            }
        }
    }

    pub(crate) fn split_anchor(&self) -> (PaneId, PaneId) {
        match self {
            Self::Split { first, second, .. } => (first.first_pane(), second.first_pane()),
            Self::Pane(_) => unreachable!("pane leaves do not own split handles"),
        }
    }

    pub(crate) fn parent_split_direction(&self, pane_id: PaneId) -> Option<SplitDirection> {
        match self {
            Self::Pane(_) => None,
            Self::Split {
                direction,
                first,
                second,
                ..
            } => {
                if matches!(first.as_ref(), Self::Pane(id) if *id == pane_id)
                    || matches!(second.as_ref(), Self::Pane(id) if *id == pane_id)
                {
                    Some(*direction)
                } else {
                    first
                        .parent_split_direction(pane_id)
                        .or_else(|| second.parent_split_direction(pane_id))
                }
            }
        }
    }

    pub(crate) fn split_values(
        &self,
        first_anchor: PaneId,
        second_anchor: PaneId,
    ) -> Option<(SplitDirection, f32)> {
        match self {
            Self::Pane(_) => None,
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                if first.first_pane() == first_anchor && second.first_pane() == second_anchor {
                    Some((*direction, *ratio))
                } else {
                    first
                        .split_values(first_anchor, second_anchor)
                        .or_else(|| second.split_values(first_anchor, second_anchor))
                }
            }
        }
    }

    pub(crate) fn set_split_ratio(
        &mut self,
        first_anchor: PaneId,
        second_anchor: PaneId,
        ratio: f32,
    ) -> bool {
        match self {
            Self::Pane(_) => false,
            Self::Split {
                ratio: current,
                first,
                second,
                ..
            } => {
                if first.first_pane() == first_anchor && second.first_pane() == second_anchor {
                    *current = clamp_ratio(ratio);
                    true
                } else {
                    first.set_split_ratio(first_anchor, second_anchor, ratio)
                        || second.set_split_ratio(first_anchor, second_anchor, ratio)
                }
            }
        }
    }

    pub(crate) fn toggle_split_direction(
        &mut self,
        first_anchor: PaneId,
        second_anchor: PaneId,
    ) -> bool {
        match self {
            Self::Pane(_) => false,
            Self::Split {
                direction,
                first,
                second,
                ..
            } => {
                if first.first_pane() == first_anchor && second.first_pane() == second_anchor {
                    *direction = match direction {
                        SplitDirection::Horizontal => SplitDirection::Vertical,
                        SplitDirection::Vertical => SplitDirection::Horizontal,
                    };
                    true
                } else {
                    first.toggle_split_direction(first_anchor, second_anchor)
                        || second.toggle_split_direction(first_anchor, second_anchor)
                }
            }
        }
    }

    pub(crate) fn split_next_to(
        &mut self,
        source: PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
        after: bool,
    ) -> bool {
        match self {
            Self::Pane(id) if *id == source => {
                let existing = Self::Pane(source);
                let new = Self::Pane(new_pane);
                let (first, second) = if after {
                    (existing, new)
                } else {
                    (new, existing)
                };
                *self = Self::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split_next_to(source, new_pane, direction, after)
                    || second.split_next_to(source, new_pane, direction, after)
            }
        }
    }

    pub(crate) fn remove_pane(&mut self, pane_id: PaneId) -> Option<PaneId> {
        if self.pane_ids().len() <= 1 || !self.contains(pane_id) {
            return None;
        }
        let (replacement, neighbor) = self.clone().without(pane_id);
        *self = replacement.expect("more than one pane remains after removal");
        neighbor
    }

    /// Remove an existing leaf and insert that same leaf beside another one.
    ///
    /// Build the replacement on a copy so a failed target lookup cannot leave
    /// the live layout with the source removed.
    pub(crate) fn relocate_pane_split(
        &mut self,
        source: PaneId,
        target: PaneId,
        direction: SplitDirection,
        after: bool,
    ) -> bool {
        if source == target || !self.contains(source) || !self.contains(target) {
            return false;
        }

        let mut replacement = self.clone();
        if replacement.remove_pane(source).is_none()
            || !replacement.split_next_to(target, source, direction, after)
        {
            return false;
        }

        *self = replacement;
        true
    }

    fn without(self, pane_id: PaneId) -> (Option<Self>, Option<PaneId>) {
        match self {
            Self::Pane(id) if id == pane_id => (None, None),
            Self::Pane(id) => (Some(Self::Pane(id)), None),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                if first.contains(pane_id) {
                    let (first, neighbor) = first.without(pane_id);
                    match first {
                        Some(first) => (
                            Some(Self::Split {
                                direction,
                                ratio,
                                first: Box::new(first),
                                second,
                            }),
                            neighbor,
                        ),
                        None => {
                            let neighbor = second.first_pane();
                            (Some(*second), Some(neighbor))
                        }
                    }
                } else if second.contains(pane_id) {
                    let (second, neighbor) = second.without(pane_id);
                    match second {
                        Some(second) => (
                            Some(Self::Split {
                                direction,
                                ratio,
                                first,
                                second: Box::new(second),
                            }),
                            neighbor,
                        ),
                        None => {
                            let neighbor = first.last_pane();
                            (Some(*first), Some(neighbor))
                        }
                    }
                } else {
                    (
                        Some(Self::Split {
                            direction,
                            ratio,
                            first,
                            second,
                        }),
                        None,
                    )
                }
            }
        }
    }

    pub(crate) fn swap_panes(&mut self, first_id: PaneId, second_id: PaneId) -> bool {
        if first_id == second_id || !self.contains(first_id) || !self.contains(second_id) {
            return false;
        }
        self.swap_leaf_ids(first_id, second_id);
        true
    }

    fn swap_leaf_ids(&mut self, first_id: PaneId, second_id: PaneId) {
        match self {
            Self::Pane(id) if *id == first_id => *id = second_id,
            Self::Pane(id) if *id == second_id => *id = first_id,
            Self::Pane(_) => {}
            Self::Split { first, second, .. } => {
                first.swap_leaf_ids(first_id, second_id);
                second.swap_leaf_ids(first_id, second_id);
            }
        }
    }
}

pub(crate) fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.02, 0.98)
    } else {
        0.5
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneId, PaneLayoutNode as Node, SplitDirection};

    #[test]
    fn split_resize_direction_swap_and_remove_keep_tree_order_consistent() {
        let mut layout = Node::single(PaneId(1));
        assert!(layout.split_next_to(PaneId(1), PaneId(2), SplitDirection::Horizontal, true,));
        assert!(layout.split_next_to(PaneId(2), PaneId(3), SplitDirection::Vertical, true,));
        assert_eq!(layout.pane_ids(), [PaneId(1), PaneId(2), PaneId(3)]);

        assert!(layout.set_split_ratio(PaneId(2), PaneId(3), 0.75));
        assert_eq!(layout.split_values(PaneId(2), PaneId(3)).unwrap().1, 0.75);
        assert!(layout.toggle_split_direction(PaneId(2), PaneId(3)));
        assert_eq!(
            layout.split_values(PaneId(2), PaneId(3)).unwrap().0,
            SplitDirection::Horizontal
        );

        assert!(layout.swap_panes(PaneId(1), PaneId(3)));
        assert_eq!(layout.pane_ids(), [PaneId(3), PaneId(2), PaneId(1)]);
        assert_eq!(layout.remove_pane(PaneId(2)), Some(PaneId(1)));
        assert_eq!(layout.pane_ids(), [PaneId(3), PaneId(1)]);
        assert_eq!(layout.remove_pane(PaneId(3)), Some(PaneId(1)));
        assert_eq!(layout.pane_ids(), [PaneId(1)]);
        assert_eq!(layout.remove_pane(PaneId(1)), None);
    }

    #[test]
    fn nested_width_fractions_follow_horizontal_ratios_and_ignore_vertical_splits() {
        let layout = Node::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.6,
            first: Box::new(Node::Pane(PaneId(1))),
            second: Box::new(Node::Split {
                direction: SplitDirection::Vertical,
                ratio: 0.25,
                first: Box::new(Node::Pane(PaneId(2))),
                second: Box::new(Node::Pane(PaneId(3))),
            }),
        };

        let fractions = layout.width_fractions();
        assert_eq!(fractions.len(), 3);
        assert_eq!(fractions[0].0, PaneId(1));
        assert_eq!(fractions[1].0, PaneId(2));
        assert_eq!(fractions[2].0, PaneId(3));
        assert!((fractions[0].1 - 0.6).abs() < f32::EPSILON);
        assert!((fractions[1].1 - 0.4).abs() < f32::EPSILON);
        assert!((fractions[2].1 - 0.4).abs() < f32::EPSILON);
    }
}
