//! v5 と同じ、folder item / breadcrumb から開く横カスケードの tree menu。
//!
//! DirectorySession を tree menu の列に流用しない。menu は開いている間だけの
//! disposable projection であり、各列の detached read と ticket をここで所有する。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_MENU_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_folder: bool,
    pub expandable: bool,
    pub is_hidden: bool,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub menu_generation: u64,
    pub column_id: u64,
    pub request_id: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub enum ColumnState {
    Loading,
    Ready(Arc<[Entry]>),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Column {
    pub id: u64,
    pub path: PathBuf,
    pub state: ColumnState,
    request_id: u64,
}

#[derive(Default)]
pub struct Model {
    generation: u64,
    next_column_id: u64,
    next_request_id: u64,
    columns: Vec<Column>,
}

impl Model {
    pub fn open(&mut self, path: PathBuf) -> Request {
        // Menu generation must outlive Model instances: closing and reopening the
        // same path creates a new Model while detached enumeration may still finish.
        self.generation = NEXT_MENU_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.columns.clear();
        self.push_column(path)
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn hover_folder(&mut self, column_index: usize, entry: &Entry) -> Option<Request> {
        if !entry.is_folder || !entry.expandable {
            self.columns.truncate(column_index.saturating_add(1));
            return None;
        }
        if self
            .columns
            .get(column_index + 1)
            .is_some_and(|column| column.path == entry.path)
        {
            return None;
        }
        self.columns.truncate(column_index.saturating_add(1));
        Some(self.push_column(entry.path.clone()))
    }

    pub fn complete(
        &mut self,
        request: &Request,
        result: Result<Vec<Entry>, String>,
        show_hidden_items: bool,
    ) -> bool {
        if request.menu_generation != self.generation {
            return false;
        }
        let Some(column) = self
            .columns
            .iter_mut()
            .find(|column| column.id == request.column_id)
        else {
            return false;
        };
        if column.request_id != request.request_id || column.path != request.path {
            return false;
        }
        column.state = match result {
            Ok(mut entries) => {
                if !show_hidden_items {
                    entries.retain(|entry| !entry.is_hidden);
                }
                ColumnState::Ready(entries.into())
            }
            Err(error) => ColumnState::Error(error),
        };
        true
    }

    fn push_column(&mut self, path: PathBuf) -> Request {
        let column_id = self.next_column_id;
        self.next_column_id = self.next_column_id.wrapping_add(1);
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.columns.push(Column {
            id: column_id,
            path: path.clone(),
            state: ColumnState::Loading,
            request_id,
        });
        Request {
            menu_generation: self.generation,
            column_id,
            request_id,
            path,
        }
    }
}

/// menu 専用の軽量 read。listing の metadata/detail/previews を作らない。
pub fn enumerate(path: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(path).map_err(|error| error.to_string())? {
        let item = match item {
            Ok(item) => item,
            Err(_) => continue,
        };
        let link_metadata = match std::fs::symlink_metadata(item.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let metadata = match item.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let is_folder = metadata.is_dir();
        let reparse = item
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(true)
            || is_reparse_point(&metadata);
        entries.push(Entry {
            path: item.path(),
            name: item.file_name().to_string_lossy().into_owned(),
            is_folder,
            expandable: is_folder && !reparse,
            is_hidden: metadata_is_hidden(&link_metadata),
        });
    }
    // Keep the potentially large sort on the detached enumeration task. The
    // foreground completion path only validates identity and installs the result.
    entries.sort_by_cached_key(|entry| (!entry.is_folder, entry.name.to_lowercase()));
    Ok(entries)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn folder(path: &str) -> Entry {
        Entry {
            path: path.into(),
            name: path.into(),
            is_folder: true,
            expandable: true,
            is_hidden: false,
        }
    }
    #[test]
    fn late_column_completion_after_hovering_another_folder_is_rejected() {
        let mut model = Model::default();
        let root = model.open(r"C:\".into());
        model.complete(&root, Ok(vec![]), false);
        let first = model.hover_folder(0, &folder(r"C:\one")).unwrap();
        let second = model.hover_folder(0, &folder(r"C:\two")).unwrap();
        assert!(!model.complete(&first, Ok(vec![]), false));
        assert!(model.complete(&second, Ok(vec![]), false));
    }

    #[test]
    fn hovering_same_folder_preserves_descendants_and_does_not_request_again() {
        let mut model = Model::default();
        let root = model.open(r"C:\".into());
        model.complete(&root, Ok(vec![]), false);
        let first = model.hover_folder(0, &folder(r"C:\one")).unwrap();
        model.complete(&first, Ok(vec![]), false);
        let descendant = model.hover_folder(1, &folder(r"C:\one\nested")).unwrap();
        model.complete(&descendant, Ok(vec![]), false);

        assert!(model.hover_folder(0, &folder(r"C:\one")).is_none());
        assert_eq!(model.columns().len(), 3);
        assert_eq!(model.columns()[2].path, PathBuf::from(r"C:\one\nested"));
    }

    #[test]
    fn hovering_file_truncates_descendants_without_requesting_enumeration() {
        let mut model = Model::default();
        let root = model.open(r"C:\".into());
        model.complete(&root, Ok(vec![]), false);
        let child = model.hover_folder(0, &folder(r"C:\one")).unwrap();
        model.complete(&child, Ok(vec![]), false);
        let grandchild = model.hover_folder(1, &folder(r"C:\one\nested")).unwrap();
        model.complete(&grandchild, Ok(vec![]), false);
        let file = Entry {
            path: r"C:\one\note.txt".into(),
            name: "note.txt".into(),
            is_folder: false,
            expandable: false,
            is_hidden: false,
        };

        assert!(model.hover_folder(1, &file).is_none());
        assert_eq!(model.columns().len(), 2);
        assert_eq!(model.columns()[1].path, PathBuf::from(r"C:\one"));
    }

    #[test]
    fn completion_from_closed_menu_is_rejected_after_reopening_same_path() {
        let mut old_menu = Model::default();
        let stale = old_menu.open(r"C:\same".into());
        let mut new_menu = Model::default();
        let current = new_menu.open(r"C:\same".into());

        assert!(!new_menu.complete(&stale, Ok(vec![]), false));
        assert!(new_menu.complete(&current, Ok(vec![]), false));
    }

    #[test]
    fn accepted_completion_filters_hidden_children_with_the_current_preference() {
        let mut model = Model::default();
        let request = model.open(r"C:\parent".into());
        let mut hidden = folder(r"C:\parent\hidden");
        hidden.is_hidden = true;
        let visible = folder(r"C:\parent\visible");

        assert!(model.complete(&request, Ok(vec![hidden.clone(), visible.clone()]), false));
        let ColumnState::Ready(entries) = &model.columns()[0].state else {
            panic!("accepted enumeration should be ready");
        };
        assert_eq!(entries.as_ref(), &[visible.clone()]);

        let show_request = model.open(r"C:\parent".into());
        assert!(model.complete(&show_request, Ok(vec![hidden.clone(), visible]), true));
        let ColumnState::Ready(entries) = &model.columns()[0].state else {
            panic!("accepted enumeration should be ready");
        };
        assert_eq!(entries.len(), 2);
    }
}

#[cfg(windows)]
fn metadata_is_hidden(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes() & 0x2 != 0 // FILE_ATTRIBUTE_HIDDEN
}

#[cfg(not(windows))]
fn metadata_is_hidden(_: &std::fs::Metadata) -> bool {
    false
}
