//! フォルダー単位の表示設定と、その保存形式。

use crate::session_store::{GroupValue, LayoutValue, PathValue, SortValue};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 16_384;
// UTF-16 units serialize as decimal JSON numbers (up to five digits plus a comma).
// Leave generous room for each record's field names, preference values, and envelope.
const MAX_RECORD_ESTIMATE: usize = 288;
const MAX_FILE_ENVELOPE_ESTIMATE: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FolderViewPrefs {
    pub layout: LayoutValue,
    pub sort: SortValue,
    pub group: GroupValue,
    #[serde(default)]
    pub columns: ColumnPrefs,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ColumnPrefs {
    pub kind: bool,
    pub size: bool,
    pub modified: bool,
    #[serde(default = "default_kind_width", deserialize_with = "deserialize_width")]
    pub kind_width: f32,
    #[serde(default = "default_size_width", deserialize_with = "deserialize_width")]
    pub size_width: f32,
    #[serde(
        default = "default_modified_width",
        deserialize_with = "deserialize_width"
    )]
    pub modified_width: f32,
    #[serde(default = "default_order", deserialize_with = "deserialize_order")]
    pub order: [crate::sorting::Column; 4],
}

fn default_order() -> [crate::sorting::Column; 4] {
    use crate::sorting::Column::*;
    [Name, Kind, Size, Modified]
}

fn default_kind_width() -> f32 {
    80.0
}
fn default_size_width() -> f32 {
    80.0
}
fn default_modified_width() -> f32 {
    120.0
}

fn deserialize_width<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let width = f32::deserialize(deserializer)?;
    Ok(if width.is_finite() {
        width.clamp(50.0, 500.0)
    } else {
        80.0
    })
}

impl ColumnPrefs {
    pub fn normalized(self) -> Self {
        Self {
            kind_width: normalize_width(self.kind_width, 80.0),
            size_width: normalize_width(self.size_width, 80.0),
            modified_width: normalize_width(self.modified_width, 120.0),
            ..self
        }
    }
}

fn normalize_width(width: f32, default: f32) -> f32 {
    if width.is_finite() {
        width.clamp(50.0, 500.0)
    } else {
        default
    }
}

fn deserialize_order<'de, D>(deserializer: D) -> Result<[crate::sorting::Column; 4], D::Error>
where
    D: serde::Deserializer<'de>,
{
    use crate::sorting::Column;
    let order = Vec::<Column>::deserialize(deserializer)?;
    let valid = order.len() == 4
        && [Column::Name, Column::Kind, Column::Size, Column::Modified]
            .into_iter()
            .all(|column| order.iter().filter(|&&item| item == column).count() == 1);
    if valid {
        Ok(order.try_into().expect("validated four columns"))
    } else {
        Ok(default_order())
    }
}

impl Default for ColumnPrefs {
    fn default() -> Self {
        Self {
            kind: true,
            size: true,
            modified: true,
            kind_width: default_kind_width(),
            size_width: default_size_width(),
            modified_width: default_modified_width(),
            order: default_order(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ChangedField {
    Layout,
    Sort,
    Group,
    Column {
        column: crate::sorting::Column,
        visible: bool,
    },
    Order {
        order: [crate::sorting::Column; 4],
    },
    Width {
        column: crate::sorting::Column,
        width: f32,
    },
}

impl FolderViewPrefs {
    pub fn with_field(self, incoming: Self, field: ChangedField) -> Self {
        match field {
            ChangedField::Layout => Self {
                layout: incoming.layout,
                ..self
            },
            ChangedField::Sort => Self {
                sort: incoming.sort,
                ..self
            },
            ChangedField::Group => Self {
                group: incoming.group,
                ..self
            },
            ChangedField::Column { column, visible } => {
                let mut columns = self.columns;
                match column {
                    crate::sorting::Column::Kind => columns.kind = visible,
                    crate::sorting::Column::Size => columns.size = visible,
                    crate::sorting::Column::Modified => columns.modified = visible,
                    crate::sorting::Column::Name => {}
                }
                Self { columns, ..self }
            }
            ChangedField::Order { order } => Self {
                columns: ColumnPrefs {
                    order,
                    ..self.columns
                },
                ..self
            },
            ChangedField::Width { column, width } => {
                let mut columns = self.columns;
                let width = normalize_width(
                    width,
                    match column {
                        crate::sorting::Column::Kind => 80.0,
                        crate::sorting::Column::Size => 80.0,
                        crate::sorting::Column::Modified => 120.0,
                        crate::sorting::Column::Name => return self,
                    },
                );
                match column {
                    crate::sorting::Column::Kind => columns.kind_width = width,
                    crate::sorting::Column::Size => columns.size_width = width,
                    crate::sorting::Column::Modified => columns.modified_width = width,
                    crate::sorting::Column::Name => {}
                }
                Self { columns, ..self }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    path: PathValue,
    prefs: FolderViewPrefs,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema_version: u32,
    revision: u64,
    records: Vec<Record>,
}

#[derive(Debug)]
pub(crate) enum LoadError {
    Unreadable(String),
    FutureSchema(u32),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    pub revision: u64,
    pub records: BTreeMap<Vec<u16>, FolderViewPrefs>,
    estimated_bytes: usize,
}

impl Snapshot {
    pub fn get(&self, path: &Path) -> Option<FolderViewPrefs> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            self.records
                .get(&path.as_os_str().encode_wide().collect::<Vec<_>>())
                .copied()
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            None
        }
    }

    pub fn insert(&mut self, path: &Path, prefs: FolderViewPrefs) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            let units: Vec<_> = path.as_os_str().encode_wide().collect();
            if units.is_empty()
                || units.len() > crate::session_store::MAX_PATH_UNITS
                || units.contains(&0)
                || !path.is_absolute()
            {
                return false;
            }
            if let Some(existing) = self.records.get_mut(&units) {
                if *existing == prefs {
                    return false;
                }
                *existing = prefs;
                return true;
            }
            let estimate = units
                .len()
                .saturating_mul(6)
                .saturating_add(MAX_RECORD_ESTIMATE);
            let budget = MAX_BYTES.saturating_sub(MAX_FILE_ENVELOPE_ESTIMATE);
            if self.records.len() >= MAX_RECORDS
                || self.estimated_bytes.saturating_add(estimate) > budget
            {
                return false;
            }
            self.estimated_bytes = self.estimated_bytes.saturating_add(estimate);
            self.records.insert(units, prefs);
            true
        }
        #[cfg(not(windows))]
        {
            let _ = (path, prefs);
            false
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.records.len() > MAX_RECORDS {
            return Err("folder preference record limit exceeded".into());
        }
        let records = self
            .records
            .iter()
            .map(|(units, prefs)| Record {
                path: PathValue::WindowsUtf16 {
                    units: units.clone(),
                },
                prefs: *prefs,
            })
            .collect();
        let file = File {
            schema_version: SCHEMA_VERSION,
            revision: self.revision,
            records,
        };
        let bytes = serde_json::to_vec(&file).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("folder preference file exceeds size limit".into());
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, LoadError> {
        if bytes.len() > MAX_BYTES {
            return Err(LoadError::Unreadable(
                "folder preferences exceed size limit".into(),
            ));
        }
        let file: File = serde_json::from_slice(bytes)
            .map_err(|error| LoadError::Unreadable(error.to_string()))?;
        if file.schema_version > SCHEMA_VERSION {
            return Err(LoadError::FutureSchema(file.schema_version));
        }
        if file.schema_version != SCHEMA_VERSION || file.records.len() > MAX_RECORDS {
            return Err(LoadError::Unreadable(
                "unsupported or oversized folder preferences".into(),
            ));
        }
        let mut records = BTreeMap::new();
        let mut estimated_bytes = 0usize;
        for record in file.records {
            record
                .path
                .validate()
                .map_err(|error| LoadError::Unreadable(error.to_string()))?;
            let PathValue::WindowsUtf16 { units } = record.path;
            estimated_bytes = estimated_bytes.saturating_add(
                units
                    .len()
                    .saturating_mul(6)
                    .saturating_add(MAX_RECORD_ESTIMATE),
            );
            if records.insert(units, record.prefs).is_some() {
                return Err(LoadError::Unreadable("duplicate folder path".into()));
            }
        }
        let write_budget = MAX_BYTES.saturating_sub(MAX_FILE_ENVELOPE_ESTIMATE);
        if estimated_bytes > write_budget {
            return Err(LoadError::Unreadable(
                "folder preference data was preserved because it leaves no safe write headroom"
                    .into(),
            ));
        }
        Ok(Self {
            revision: file.revision,
            records,
            estimated_bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StoreFile {
    path: PathBuf,
}

impl StoreFile {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_for_app() -> Option<Self> {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| Self {
                path: root.join("gpui-filer/v6/folder-view-preferences-v1.json"),
            })
    }

    pub fn load(&self) -> Result<Snapshot, LoadError> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Snapshot::default());
            }
            Err(error) => return Err(LoadError::Unreadable(error.to_string())),
        };
        let mut bytes = Vec::new();
        file.by_ref()
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| LoadError::Unreadable(error.to_string()))?;
        Snapshot::decode(&bytes)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_BYTES {
            return Err("folder preference file exceeds size limit".into());
        }
        let parent = self
            .path
            .parent()
            .ok_or("folder preference path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, bytes).map_err(|error| error.to_string())?;
        // Same-directory rename replaces atomically on the supported Windows target.
        std::fs::rename(&temp, &self.path).map_err(|error| error.to_string())
    }
}

pub(crate) struct WriteRequest {
    pub revision: u64,
    pub bytes: Vec<u8>,
}
pub(crate) struct WriteCompletion {
    pub revision: u64,
    pub result: Result<(), String>,
}

pub(crate) struct Writer {
    queue: Arc<(Mutex<WriteQueue>, Condvar)>,
    pub completions: mpsc::Receiver<WriteCompletion>,
}

#[derive(Default)]
struct WriteQueue {
    pending: Option<WriteRequest>,
    latest_requested: u64,
    written_revision: u64,
    stopped: bool,
}

impl Writer {
    pub fn start(file: StoreFile) -> Self {
        // 進行中1件 + pending 1件に固定し、pending は常に最新 snapshot へ置き換える。
        let queue = Arc::new((Mutex::new(WriteQueue::default()), Condvar::new()));
        let worker_queue = queue.clone();
        let (completed, completions) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let request = {
                    let (lock, wake) = &*worker_queue;
                    let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    while state.pending.is_none() && !state.stopped {
                        state = wake
                            .wait(state)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    if let Some(request) = state.pending.take() {
                        request
                    } else {
                        break;
                    }
                };
                let result = file.write(&request.bytes);
                if result.is_ok() {
                    let (lock, _) = &*worker_queue;
                    let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.written_revision = state.written_revision.max(request.revision);
                }
                let _ = completed.send(WriteCompletion {
                    revision: request.revision,
                    result,
                });
            }
        });
        Self { queue, completions }
    }

    pub fn request(&self, revision: u64, bytes: Vec<u8>) -> Result<(), String> {
        let (lock, wake) = &*self.queue;
        let mut state = match lock.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err("folder preference writer queue is busy".into());
            }
        };
        if state.stopped {
            return Err("folder preference writer stopped".into());
        }
        if revision < state.latest_requested || revision < state.written_revision {
            return Err("stale folder preference write revision rejected".into());
        }
        state.latest_requested = state.latest_requested.max(revision);
        state.pending = Some(WriteRequest { revision, bytes });
        wake.notify_one();
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let (lock, wake) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stopped = true;
        wake.notify_all();
    }
}
