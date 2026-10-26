//! Pure batch-rename planning. This module only expands names and compares
//! accepted listing facts; workers still revalidate every filesystem identity.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameSource {
    pub source: PathBuf,
    pub name: String,
    pub kind: crate::directory::EntryKind,
    pub identity: crate::filesystem_identity::FilesystemIdentity,
    pub reparse_guard: Option<crate::operations::model::RenameReparseGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameTarget {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub kind: crate::directory::EntryKind,
    pub reparse_guard: Option<crate::operations::model::RenameReparseGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewRow {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub error: Option<&'static str>,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub rows: Vec<PreviewRow>,
    pub can_submit: bool,
    pub no_change: bool,
}

/// `{name}` is the stem, `{ext}` includes its leading dot, and `{n}` is the
/// one-based position in the captured display-order snapshot.
pub fn preview(
    sources: &[RenameSource],
    pattern: &str,
    find: &str,
    replace: &str,
    existing_names: &[String],
) -> Preview {
    let mut rows = Vec::with_capacity(sources.len());
    let mut planned = Vec::<String>::with_capacity(sources.len());
    let mut any_changed = false;
    for (index, source) in sources.iter().enumerate() {
        let (stem, ext) = if source.kind == crate::directory::EntryKind::Folder {
            (source.name.as_str(), "")
        } else {
            split_name(&source.name)
        };
        let expanded = pattern
            .replace("{name}", stem)
            .replace("{ext}", ext)
            .replace("{n}", &(index + 1).to_string());
        let target_name = if find.is_empty() {
            expanded
        } else {
            expanded.replace(find, replace)
        };
        let destination = source
            .source
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .join(&target_name);
        let changed = !source
            .source
            .file_name()
            .is_some_and(|name| name.to_string_lossy() == target_name);
        any_changed |= changed;
        let folded = target_name.to_lowercase();
        let collision = planned.iter().any(|other| other == &folded)
            || existing_names.iter().any(|other| {
                other.to_lowercase() == folded
                    && !source
                        .source
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(other))
            });
        let error = if !crate::operations::rename::is_single_name(&target_name) {
            Some("Invalid name")
        } else if collision {
            Some("Name already exists or is duplicated")
        } else {
            None
        };
        planned.push(folded);
        rows.push(PreviewRow {
            source: source.source.clone(),
            destination,
            error,
            changed,
        });
    }
    let can_submit = !sources.is_empty() && rows.iter().all(|row| row.error.is_none());
    Preview {
        rows,
        can_submit,
        no_change: can_submit && !any_changed,
    }
}

fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(index) => (&name[..index], &name[index..]),
    }
}
