//! Validated, detached browsing metadata shared by archive readers.

use std::cmp::Ordering;
use std::collections::HashSet;

const MAX_RECORDS: usize = 65_536;
const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 128;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestNode {
    pub(crate) components: Vec<String>,
    pub(crate) kind: NodeKind,
    pub(crate) explicit: bool,
    pub(crate) declared_size: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ArchiveManifest {
    pub(crate) nodes: Vec<ManifestNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawRecord {
    pub(crate) components: Vec<String>,
    pub(crate) kind: NodeKind,
    pub(crate) declared_size: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ManifestError(pub(crate) &'static str);

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl ArchiveManifest {
    pub(crate) fn validate(
        records: impl IntoIterator<Item = RawRecord>,
    ) -> Result<Self, ManifestError> {
        let records = records.into_iter();
        let mut raw = Vec::new();
        let mut raw_name_bytes = 0usize;
        for record in records {
            if raw.len() == MAX_RECORDS {
                return Err(ManifestError("archive listing record bound exceeded"));
            }
            if record.components.is_empty()
                || record.components.len() > MAX_DEPTH
                || record
                    .components
                    .iter()
                    .any(|part| !crate::windows_name::is_single_name(part))
            {
                return Err(ManifestError("invalid archive listing path"));
            }
            if record.kind == NodeKind::Directory && record.declared_size.unwrap_or(0) != 0 {
                return Err(ManifestError("archive directory declares data"));
            }
            let charge = path_name_bytes(&record.components)?;
            raw_name_bytes = raw_name_bytes
                .checked_add(charge)
                .ok_or(ManifestError("archive listing raw name bound overflow"))?;
            if raw_name_bytes > MAX_NAME_BYTES {
                return Err(ManifestError("archive listing raw name bound exceeded"));
            }
            raw.push(record);
        }

        // 全体走査を避け、並べ替え後の隣接比較で重複を O(n log n) に抑える。
        raw.sort_by(|a, b| compare_paths(&a.components, &b.components));
        for pair in raw.windows(2) {
            if compare_paths(&pair[0].components, &pair[1].components) == Ordering::Equal {
                return Err(ManifestError("duplicate or case-colliding archive path"));
            }
        }

        let mut candidates = Vec::<ManifestNode>::new();
        let mut candidate_bytes = 0usize;
        let mut seen_paths = HashSet::<Vec<String>>::new();
        for record in raw {
            for depth in 1..record.components.len() {
                let prefix = &record.components[..depth];
                if !seen_paths.contains(prefix) {
                    let charge = path_name_bytes(prefix)?;
                    check_candidate_budget(candidates.len(), candidate_bytes, charge)?;
                    let components = prefix.to_vec();
                    seen_paths.insert(components.clone());
                    push_candidate(
                        &mut candidates,
                        ManifestNode {
                            components,
                            kind: NodeKind::Directory,
                            explicit: false,
                            declared_size: None,
                        },
                        &mut candidate_bytes,
                    )?;
                }
            }
            push_candidate(
                &mut candidates,
                ManifestNode {
                    components: record.components,
                    kind: record.kind,
                    explicit: true,
                    declared_size: (record.kind == NodeKind::File)
                        .then_some(record.declared_size)
                        .flatten(),
                },
                &mut candidate_bytes,
            )?;
        }
        candidates.sort_by(|a, b| compare_paths(&a.components, &b.components));
        let mut nodes = Vec::<ManifestNode>::new();
        let mut name_bytes = 0usize;
        for candidate in candidates {
            if let Some(previous) = nodes.last_mut().filter(|node| {
                compare_paths(&node.components, &candidate.components) == Ordering::Equal
            }) {
                if previous.components != candidate.components {
                    return Err(ManifestError("case-colliding archive path components"));
                }
                if previous.kind != candidate.kind || (previous.explicit && candidate.explicit) {
                    return Err(ManifestError("archive file/directory prefix conflict"));
                }
                if candidate.explicit {
                    previous.explicit = true;
                    previous.declared_size = candidate.declared_size;
                }
            } else {
                push_node(&mut nodes, candidate, &mut name_bytes)?;
            }
        }
        // 暗黙ディレクトリも揃った後で親を引き、入力順に依存せずファイル衝突を調べる。
        for node in &nodes {
            for depth in 1..node.components.len() {
                if let Ok(index) = nodes.binary_search_by(|candidate| {
                    compare_paths(&candidate.components, &node.components[..depth])
                }) {
                    if nodes[index].kind == NodeKind::File {
                        return Err(ManifestError("archive file/directory prefix conflict"));
                    }
                }
            }
        }
        Ok(Self { nodes })
    }
}

fn push_node(
    nodes: &mut Vec<ManifestNode>,
    node: ManifestNode,
    name_bytes: &mut usize,
) -> Result<(), ManifestError> {
    if nodes.len() >= MAX_NODES {
        return Err(ManifestError("archive listing node bound exceeded"));
    }
    let charge = path_name_bytes(&node.components)?;
    *name_bytes = name_bytes
        .checked_add(charge)
        .ok_or(ManifestError("archive listing name bound overflow"))?;
    if *name_bytes > MAX_NAME_BYTES {
        return Err(ManifestError("archive listing name bound exceeded"));
    }
    nodes.push(node);
    Ok(())
}

fn push_candidate(
    candidates: &mut Vec<ManifestNode>,
    node: ManifestNode,
    candidate_bytes: &mut usize,
) -> Result<(), ManifestError> {
    let charge = path_name_bytes(&node.components)?;
    check_candidate_budget(candidates.len(), *candidate_bytes, charge)?;
    *candidate_bytes += charge;
    candidates.push(node);
    Ok(())
}

fn check_candidate_budget(
    candidate_count: usize,
    candidate_bytes: usize,
    charge: usize,
) -> Result<(), ManifestError> {
    if candidate_count >= MAX_NODES.saturating_mul(2) {
        return Err(ManifestError("archive listing candidate bound exceeded"));
    }
    let next_bytes = candidate_bytes.checked_add(charge).ok_or(ManifestError(
        "archive listing candidate byte bound overflow",
    ))?;
    if next_bytes > MAX_NAME_BYTES.saturating_mul(2) {
        return Err(ManifestError(
            "archive listing candidate byte bound exceeded",
        ));
    }
    Ok(())
}

fn path_name_bytes(components: &[String]) -> Result<usize, ManifestError> {
    let component_bytes = components
        .iter()
        .try_fold(0usize, |sum, part| sum.checked_add(part.len()))
        .ok_or(ManifestError("archive listing name bound overflow"))?;
    component_bytes
        .checked_add(components.len().saturating_sub(1))
        .ok_or(ManifestError("archive listing name bound overflow"))
}

pub(crate) fn compare_paths(a: &[String], b: &[String]) -> Ordering {
    for (left, right) in a.iter().zip(b) {
        let order = compare_component(left, right);
        if order != Ordering::Equal {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

fn compare_component(a: &str, b: &str) -> Ordering {
    #[cfg(windows)]
    {
        use windows::Win32::{Foundation::BOOL, Globalization::CompareStringOrdinal};
        let (aw, bw) = (
            a.encode_utf16().collect::<Vec<_>>(),
            b.encode_utf16().collect::<Vec<_>>(),
        );
        match unsafe { CompareStringOrdinal(&aw, &bw, BOOL(1)) }.0 {
            1 => Ordering::Less,
            2 => Ordering::Equal,
            3 => Ordering::Greater,
            _ => a.cmp(b),
        }
    }
    #[cfg(not(windows))]
    {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &[&str], kind: NodeKind) -> RawRecord {
        RawRecord {
            components: path.iter().map(|part| (*part).to_owned()).collect(),
            kind,
            declared_size: None,
        }
    }

    #[test]
    fn validates_empty_explicit_implicit_and_mixed_nodes_in_stable_order() {
        assert!(ArchiveManifest::validate([]).unwrap().nodes.is_empty());
        let tree = ArchiveManifest::validate([
            record(&["z"], NodeKind::Directory),
            record(&["日本語", "file.txt"], NodeKind::File),
            record(&["a.txt"], NodeKind::File),
        ])
        .unwrap();
        let names = tree
            .nodes
            .iter()
            .map(|node| node.components.join("/"))
            .collect::<Vec<_>>();
        let mut expected = names.clone();
        expected.sort_by(|a, b| {
            compare_paths(
                &a.split('/').map(str::to_owned).collect::<Vec<_>>(),
                &b.split('/').map(str::to_owned).collect::<Vec<_>>(),
            )
        });
        assert_eq!(names, expected);
        let implicit_parent = tree
            .nodes
            .iter()
            .find(|node| node.components == ["日本語"])
            .unwrap();
        let explicit_file = tree
            .nodes
            .iter()
            .find(|node| node.components == ["日本語", "file.txt"])
            .unwrap();
        assert!(!implicit_parent.explicit);
        assert!(explicit_file.explicit);
    }

    #[test]
    fn rejects_duplicate_alias_prefix_and_malformed_records() {
        assert!(
            ArchiveManifest::validate([
                record(&["日本"], NodeKind::File),
                record(&["日本"], NodeKind::File)
            ])
            .is_err()
        );
        assert!(
            ArchiveManifest::validate([
                record(&["Foo"], NodeKind::File),
                record(&["foo"], NodeKind::File)
            ])
            .is_err()
        );
        assert!(
            ArchiveManifest::validate([
                record(&["file"], NodeKind::File),
                record(&["file", "child"], NodeKind::File)
            ])
            .is_err()
        );
        assert!(
            ArchiveManifest::validate([
                record(&["Foo", "a"], NodeKind::File),
                record(&["foo", "b"], NodeKind::File),
            ])
            .is_err()
        );
        assert!(
            ArchiveManifest::validate([
                record(&["Foo"], NodeKind::Directory),
                record(&["foo", "child"], NodeKind::File),
            ])
            .is_err()
        );
        for part in ["", ".", "..", "C:", "a/b", "NUL.txt"] {
            assert!(
                ArchiveManifest::validate([record(&[part], NodeKind::File)]).is_err(),
                "{part:?}"
            );
        }
        let mut payload = record(&["dir"], NodeKind::Directory);
        payload.declared_size = Some(1);
        assert!(ArchiveManifest::validate([payload]).is_err());
    }

    #[test]
    fn enforces_raw_depth_and_materialized_name_bounds() {
        assert!(
            ArchiveManifest::validate((0..MAX_RECORDS).map(|index| record(
                &[Box::leak(format!("f{index}").into_boxed_str())],
                NodeKind::File
            )))
            .is_ok()
        );
        assert!(
            ArchiveManifest::validate((0..=MAX_RECORDS).map(|index| record(
                &[Box::leak(format!("r{index}").into_boxed_str())],
                NodeKind::File
            )))
            .is_err()
        );
        let deep = (0..=MAX_DEPTH).map(|_| "x").collect::<Vec<_>>();
        assert!(ArchiveManifest::validate([record(&deep, NodeKind::File)]).is_err());
        let long = "x".repeat(MAX_NAME_BYTES / 2);
        let records = [
            RawRecord {
                components: vec![long.clone()],
                kind: NodeKind::File,
                declared_size: None,
            },
            RawRecord {
                components: vec![long],
                kind: NodeKind::File,
                declared_size: None,
            },
        ];
        assert!(ArchiveManifest::validate(records).is_err());

        let root_names = (0..MAX_RECORDS).map(|index| RawRecord {
            components: vec![format!("{index:0128}")],
            kind: NodeKind::File,
            declared_size: None,
        });
        assert_eq!(
            ArchiveManifest::validate(root_names).unwrap().nodes.len(),
            MAX_RECORDS
        );

        let materialized_bytes = |last_leaf_len: usize| {
            (0..MAX_RECORDS / 2).map(move |index| RawRecord {
                components: vec![
                    format!("r{index:05}"),
                    "x".repeat(if index == MAX_RECORDS / 2 - 1 {
                        last_leaf_len
                    } else {
                        243
                    }),
                ],
                kind: NodeKind::File,
                declared_size: None,
            })
        };
        assert_eq!(
            ArchiveManifest::validate(materialized_bytes(243))
                .unwrap()
                .nodes
                .len(),
            MAX_NODES
        );
        assert!(ArchiveManifest::validate(materialized_bytes(244)).is_err());

        let calls = std::cell::Cell::new(0);
        let remaining = std::iter::from_fn(|| {
            let call = calls.get();
            calls.set(call + 1);
            match call {
                0 => Some(RawRecord {
                    components: vec!["x".repeat(MAX_NAME_BYTES + 1)],
                    kind: NodeKind::File,
                    declared_size: None,
                }),
                1 => Some(record(&["must-not-be-read"], NodeKind::File)),
                _ => None,
            }
        });
        assert!(ArchiveManifest::validate(remaining).is_err());
        assert_eq!(
            calls.get(),
            1,
            "raw byte rejection must happen before consuming the next record"
        );

        let materialized_count = |count: usize| {
            (0..count).map(|index| RawRecord {
                components: vec![format!("r{index}"), "f".to_owned()],
                kind: NodeKind::File,
                declared_size: None,
            })
        };
        assert_eq!(
            ArchiveManifest::validate(materialized_count(MAX_NODES / 2))
                .unwrap()
                .nodes
                .len(),
            MAX_NODES
        );
        assert!(ArchiveManifest::validate(materialized_count(MAX_NODES / 2 + 1)).is_err());
    }
}
