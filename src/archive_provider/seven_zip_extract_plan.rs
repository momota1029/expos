//! Bounded validation and indexing for a complete 7-Zip metadata inventory.
//!
//! This plan preserves SDK indices for a future extraction worker. It is not a
//! source handle, provider lease, or authorization to read payload bytes.

use std::{collections::HashMap, ops::Range};

use crate::archive_manifest::{ArchiveManifest, ManifestError, NodeKind, RawRecord};

const MAX_RECORDS: usize = 65_536;
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024;
const MAX_TOTAL_SIZE: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexedRecord {
    /// Original zero-based index reported by the SDK, before any sorting.
    pub(crate) index: u32,
    pub(crate) components: Vec<String>,
    pub(crate) kind: NodeKind,
    pub(crate) declared_size: Option<u64>,
    pub(crate) crc32: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Validated7zEntry {
    index: u32,
    components: Vec<String>,
    kind: NodeKind,
    /// Directory `None` and `Some(0)` are both valid and intentionally distinct
    /// here so exact SDK inventories remain comparable.
    declared_size: Option<u64>,
    crc32: Option<u32>,
    spool_range: Option<Range<u64>>,
}

impl Validated7zEntry {
    pub(crate) fn index(&self) -> u32 {
        self.index
    }

    pub(crate) fn components(&self) -> &[String] {
        &self.components
    }

    pub(crate) fn kind(&self) -> NodeKind {
        self.kind
    }

    pub(crate) fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }

    pub(crate) fn crc32(&self) -> Option<u32> {
        self.crc32
    }

    pub(crate) fn spool_range(&self) -> Option<&Range<u64>> {
        self.spool_range.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Validated7zNode {
    components: Vec<String>,
    kind: NodeKind,
    /// Explicit nodes keep their SDK identity; synthesized parents do not.
    index: Option<u32>,
    declared_size: Option<u64>,
    spool_range: Option<Range<u64>>,
}

impl Validated7zNode {
    pub(crate) fn components(&self) -> &[String] {
        &self.components
    }

    pub(crate) fn kind(&self) -> NodeKind {
        self.kind
    }

    pub(crate) fn index(&self) -> Option<u32> {
        self.index
    }

    pub(crate) fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }

    pub(crate) fn spool_range(&self) -> Option<&Range<u64>> {
        self.spool_range.as_ref()
    }
}

/// Immutable, validated metadata and non-overlapping payload-spool offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Validated7zExtractPlan {
    entries: Vec<Validated7zEntry>,
    nodes: Vec<Validated7zNode>,
    total_size: u64,
}

impl Validated7zExtractPlan {
    /// Validates a full SDK inventory before assigning its flat payload spool.
    pub(crate) fn validate(records: Vec<IndexedRecord>) -> Result<Self, ExtractPlanError> {
        if records.len() > MAX_RECORDS {
            return Err(ExtractPlanError("7-Zip inventory record bound exceeded"));
        }

        let mut raw_name_bytes = 0usize;
        for record in &records {
            if record.components.is_empty() || record.components.len() > 128 {
                return Err(ExtractPlanError("invalid archive listing path"));
            }
            if record
                .components
                .iter()
                .any(|part| !crate::windows_name::is_single_name(part))
            {
                return Err(ExtractPlanError("invalid archive listing path"));
            }
            let bytes = record
                .components
                .iter()
                .try_fold(record.components.len().saturating_sub(1), |sum, part| {
                    sum.checked_add(part.len())
                })
                .ok_or(ExtractPlanError("archive listing name bound overflow"))?;
            raw_name_bytes = raw_name_bytes
                .checked_add(bytes)
                .ok_or(ExtractPlanError("archive listing raw name bound overflow"))?;
            if raw_name_bytes > 8 * 1024 * 1024 {
                return Err(ExtractPlanError("archive listing raw name bound exceeded"));
            }
        }

        let mut records = records;
        records.sort_by_key(|record| record.index);
        for (expected, record) in records.iter().enumerate() {
            if usize::try_from(record.index).ok() != Some(expected) {
                return Err(ExtractPlanError(
                    "7-Zip inventory indices are not contiguous",
                ));
            }
        }

        let mut total_size = 0u64;
        for record in &records {
            match record.kind {
                NodeKind::File => {
                    let size = record
                        .declared_size
                        .ok_or(ExtractPlanError("7-Zip file size is unknown"))?;
                    if size > MAX_FILE_SIZE {
                        return Err(ExtractPlanError("7-Zip file size bound exceeded"));
                    }
                    total_size = total_size
                        .checked_add(size)
                        .ok_or(ExtractPlanError("7-Zip expanded size overflow"))?;
                    if total_size > MAX_TOTAL_SIZE {
                        return Err(ExtractPlanError("7-Zip total expanded size bound exceeded"));
                    }
                }
                NodeKind::Directory => {
                    if record.declared_size.unwrap_or(0) != 0 || record.crc32.is_some() {
                        return Err(ExtractPlanError("invalid 7-Zip directory metadata"));
                    }
                }
            }
        }

        // 表示用マニフェストはノードを並べ替えるため、SDK の index は元レコードと一緒に保つ。
        let manifest = ArchiveManifest::validate(records.iter().map(|record| RawRecord {
            components: record.components.clone(),
            kind: record.kind,
            declared_size: record.declared_size,
        }))
        .map_err(ExtractPlanError::from)?;

        let mut spool_offset = 0u64;
        let entries = records
            .into_iter()
            .map(|record| {
                let spool_range = if record.kind == NodeKind::File {
                    let size = record.declared_size.expect("file size validated above");
                    let start = spool_offset;
                    spool_offset += size;
                    Some(start..spool_offset)
                } else {
                    None
                };
                Validated7zEntry {
                    index: record.index,
                    components: record.components,
                    kind: record.kind,
                    declared_size: record.declared_size,
                    crc32: record.crc32,
                    spool_range,
                }
            })
            .collect::<Vec<_>>();
        debug_assert_eq!(spool_offset, total_size);

        let entries_by_path = entries
            .iter()
            .map(|entry| (entry.components.clone(), entry))
            .collect::<HashMap<_, _>>();
        let nodes = manifest
            .nodes
            .into_iter()
            .map(|node| {
                let entry = entries_by_path.get(&node.components).copied();
                Validated7zNode {
                    components: node.components,
                    kind: node.kind,
                    index: entry.map(Validated7zEntry::index),
                    declared_size: node.declared_size,
                    spool_range: entry.and_then(|entry| entry.spool_range.clone()),
                }
            })
            .collect();

        Ok(Self {
            entries,
            nodes,
            total_size,
        })
    }

    pub(crate) fn entries(&self) -> &[Validated7zEntry] {
        &self.entries
    }

    pub(crate) fn nodes(&self) -> &[Validated7zNode] {
        &self.nodes
    }

    pub(crate) fn total_size(&self) -> u64 {
        self.total_size
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtractPlanError(&'static str);

impl std::fmt::Display for ExtractPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl From<ManifestError> for ExtractPlanError {
    fn from(error: ManifestError) -> Self {
        Self(error.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: u32, path: &[&str], kind: NodeKind, size: Option<u64>) -> IndexedRecord {
        IndexedRecord {
            index,
            components: path.iter().map(|part| (*part).to_owned()).collect(),
            kind,
            declared_size: size,
            crc32: None,
        }
    }

    #[test]
    fn canonical_entries_keep_sdk_indices_and_tree_projection() {
        let plan = Validated7zExtractPlan::validate(vec![
            record(2, &["Zoo", "child"], NodeKind::File, Some(3)),
            record(0, &["z.txt"], NodeKind::File, Some(2)),
            record(1, &["Zoo"], NodeKind::Directory, Some(0)),
            record(3, &["implicit", "leaf"], NodeKind::File, Some(1)),
        ])
        .unwrap();

        assert_eq!(plan.total_size(), 6);
        assert_eq!(
            plan.entries().iter().map(|e| e.index()).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(plan.entries()[0].spool_range(), Some(&(0..2)));
        assert_eq!(plan.entries()[2].spool_range(), Some(&(2..5)));
        assert_eq!(plan.entries()[3].spool_range(), Some(&(5..6)));
        let explicit_parent = plan
            .nodes()
            .iter()
            .find(|n| n.components() == ["Zoo"])
            .unwrap();
        let file = plan
            .nodes()
            .iter()
            .find(|n| n.components() == ["Zoo", "child"])
            .unwrap();
        assert_eq!(explicit_parent.index(), Some(1));
        assert_eq!(file.index(), Some(2));
        assert_eq!(file.spool_range(), Some(&(2..5)));
        let implicit_parent = plan
            .nodes()
            .iter()
            .find(|n| n.components() == ["implicit"])
            .unwrap();
        assert_eq!(implicit_parent.index(), None);
        assert_eq!(implicit_parent.spool_range(), None);
        let implicit_leaf = plan
            .nodes()
            .iter()
            .find(|n| n.components() == ["implicit", "leaf"])
            .unwrap();
        assert_eq!(implicit_leaf.index(), Some(3));
        assert_eq!(implicit_leaf.spool_range(), Some(&(5..6)));
    }

    #[test]
    fn empty_archive_and_empty_files_have_valid_ranges() {
        let empty = Validated7zExtractPlan::validate(vec![]).unwrap();
        assert!(empty.entries().is_empty());
        assert!(empty.nodes().is_empty());
        assert_eq!(empty.total_size(), 0);

        let plan = Validated7zExtractPlan::validate(vec![
            record(0, &["a"], NodeKind::File, Some(0)),
            record(1, &["b"], NodeKind::File, Some(0)),
        ])
        .unwrap();
        assert_eq!(plan.entries()[0].spool_range(), Some(&(0..0)));
        assert_eq!(plan.entries()[1].spool_range(), Some(&(0..0)));
    }

    #[test]
    fn rejects_noncontiguous_or_repeated_indices() {
        for records in [
            vec![
                record(0, &["a"], NodeKind::File, Some(0)),
                record(0, &["b"], NodeKind::File, Some(0)),
            ],
            vec![record(1, &["a"], NodeKind::File, Some(0))],
            vec![record(u32::MAX, &["a"], NodeKind::File, Some(0))],
        ] {
            assert!(Validated7zExtractPlan::validate(records).is_err());
        }
    }

    #[test]
    fn manifest_rejects_unsafe_alias_and_prefix_names() {
        for name in ["..", "C:", "a:b", "NUL.txt", "a/b", "\\root"] {
            assert!(
                Validated7zExtractPlan::validate(vec![
                    record(0, &[name], NodeKind::File, Some(0),)
                ])
                .is_err(),
                "{name:?}"
            );
        }
        assert!(
            Validated7zExtractPlan::validate(vec![
                record(0, &["Foo"], NodeKind::File, Some(0)),
                record(1, &["foo"], NodeKind::File, Some(0)),
            ])
            .is_err()
        );
        assert!(
            Validated7zExtractPlan::validate(vec![
                record(0, &["file"], NodeKind::File, Some(0)),
                record(1, &["file", "child"], NodeKind::File, Some(0)),
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_oversized_and_inconsistent_metadata() {
        assert!(
            Validated7zExtractPlan::validate(vec![record(0, &["a"], NodeKind::File, None)])
                .is_err()
        );
        assert!(
            Validated7zExtractPlan::validate(vec![record(
                0,
                &["a"],
                NodeKind::File,
                Some(MAX_FILE_SIZE + 1)
            )])
            .is_err()
        );
        assert!(
            Validated7zExtractPlan::validate(vec![record(
                0,
                &["a"],
                NodeKind::File,
                Some(u64::MAX),
            )])
            .is_err()
        );
        assert!(
            Validated7zExtractPlan::validate(vec![record(
                0,
                &["dir"],
                NodeKind::Directory,
                Some(1)
            )])
            .is_err()
        );
        let mut directory = record(0, &["dir"], NodeKind::Directory, None);
        directory.crc32 = Some(1);
        assert!(Validated7zExtractPlan::validate(vec![directory]).is_err());
        assert!(
            Validated7zExtractPlan::validate(vec![
                record(0, &["a"], NodeKind::File, Some(MAX_FILE_SIZE)),
                record(1, &["b"], NodeKind::File, Some(MAX_FILE_SIZE)),
                record(2, &["c"], NodeKind::File, Some(1)),
            ])
            .is_err()
        );
    }

    #[test]
    fn accepts_exact_file_and_total_size_boundaries() {
        let plan = Validated7zExtractPlan::validate(vec![
            record(0, &["a"], NodeKind::File, Some(MAX_FILE_SIZE)),
            record(1, &["b"], NodeKind::File, Some(MAX_FILE_SIZE)),
        ])
        .unwrap();
        assert_eq!(plan.total_size(), MAX_TOTAL_SIZE);
        assert_eq!(
            plan.entries()[1].spool_range(),
            Some(&(MAX_FILE_SIZE..MAX_TOTAL_SIZE))
        );
    }

    #[test]
    fn raw_name_and_materialized_node_limits_are_enforced() {
        const MAX_NODE_COUNT: usize = 65_536;
        let oversized_name = "x".repeat(8 * 1024 * 1024 + 1);
        assert!(
            Validated7zExtractPlan::validate(vec![IndexedRecord {
                index: 0,
                components: vec![oversized_name],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: None,
            }])
            .is_err()
        );

        let over_node_limit = (0..(MAX_NODE_COUNT / 3 + 1))
            .map(|index| IndexedRecord {
                index: index as u32,
                components: vec![format!("root{index}"), "sub".to_owned(), "file".to_owned()],
                kind: NodeKind::File,
                declared_size: Some(0),
                crc32: None,
            })
            .collect();
        assert!(Validated7zExtractPlan::validate(over_node_limit).is_err());
    }

    #[test]
    fn limits_and_optional_file_crc_are_part_of_validated_identity() {
        let mut a = record(0, &["a"], NodeKind::File, Some(1));
        a.crc32 = Some(5);
        let b = record(1, &["b"], NodeKind::File, Some(2));
        let original = Validated7zExtractPlan::validate(vec![a.clone(), b.clone()]).unwrap();

        let reversed_input = Validated7zExtractPlan::validate(vec![b.clone(), a.clone()]).unwrap();
        assert_eq!(original, reversed_input);

        let mut swapped_a = a.clone();
        swapped_a.index = 1;
        let mut swapped_b = b.clone();
        swapped_b.index = 0;
        assert_ne!(
            original,
            Validated7zExtractPlan::validate(vec![swapped_a, swapped_b]).unwrap()
        );

        let mut renamed = a.clone();
        renamed.components = vec!["renamed".to_owned()];
        assert_ne!(
            original,
            Validated7zExtractPlan::validate(vec![renamed, b.clone()]).unwrap()
        );

        let mut resized = a.clone();
        resized.declared_size = Some(3);
        assert_ne!(
            original,
            Validated7zExtractPlan::validate(vec![resized, b.clone()]).unwrap()
        );

        let mut changed_crc = a.clone();
        changed_crc.crc32 = Some(6);
        assert_ne!(
            original,
            Validated7zExtractPlan::validate(vec![changed_crc, b]).unwrap()
        );

        let too_many = (0..=MAX_RECORDS)
            .map(|index| record(index as u32, &["f"], NodeKind::File, Some(0)))
            .collect();
        assert!(Validated7zExtractPlan::validate(too_many).is_err());

        let deep = (0..=128).map(|_| "x").collect::<Vec<_>>();
        let deep_owned = deep.iter().map(|part| *part).collect::<Vec<_>>();
        assert!(
            Validated7zExtractPlan::validate(vec![record(0, &deep_owned, NodeKind::File, Some(0))])
                .is_err()
        );
    }

    #[test]
    fn file_ranges_are_disjoint_and_zero_files_do_not_advance_the_spool() {
        let plan = Validated7zExtractPlan::validate(vec![
            record(0, &["a"], NodeKind::File, Some(2)),
            record(1, &["empty"], NodeKind::File, Some(0)),
            record(2, &["dir"], NodeKind::Directory, None),
            record(3, &["b"], NodeKind::File, Some(4)),
        ])
        .unwrap();
        assert_eq!(plan.entries()[0].spool_range(), Some(&(0..2)));
        assert_eq!(plan.entries()[1].spool_range(), Some(&(2..2)));
        assert_eq!(plan.entries()[2].spool_range(), None);
        assert_eq!(plan.entries()[3].spool_range(), Some(&(2..6)));
        assert_eq!(plan.total_size(), 6);
    }
}
