use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::operations::executor::ExecutorFact as Fact;
use crate::operations::executor::WorkerControl;
use crate::operations::model::{
    AttemptToken, ItemId, ItemOutcome, ItemReceipt, OperationKind, OperationRequest,
    ProgressCounters, RetryPlan, TerminalOutcome, TerminalReport,
};
use std::sync::mpsc::Receiver;

const CHUNK_BYTES: usize = 1024 * 1024;
const MAX_FOLDER_ARCHIVE_ENTRIES: usize = 1024;
const MAX_FOLDER_ARCHIVE_DEPTH: usize = 128;
const MAX_FOLDER_ARCHIVE_NAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ZipCreateLimits {
    entries: usize,
    depth: usize,
    name_bytes: usize,
}

#[cfg(all(test, windows))]
static ZIP_CREATE_LIMITS_FOR_TEST: std::sync::OnceLock<
    std::sync::Mutex<Vec<(PathBuf, ZipCreateLimits)>>,
> = std::sync::OnceLock::new();

fn zip_create_limits(destination: &Path) -> ZipCreateLimits {
    #[cfg(all(test, windows))]
    if let Some(limits) = ZIP_CREATE_LIMITS_FOR_TEST
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .ok()
        .and_then(|mut overrides| {
            overrides
                .iter()
                .position(|(path, _)| path == destination)
                .map(|index| overrides.remove(index).1)
        })
    {
        return limits;
    }
    let _ = destination;
    ZipCreateLimits {
        entries: MAX_FOLDER_ARCHIVE_ENTRIES,
        depth: MAX_FOLDER_ARCHIVE_DEPTH,
        name_bytes: MAX_FOLDER_ARCHIVE_NAME_BYTES,
    }
}

#[cfg(all(test, windows))]
static BEFORE_FOLDER_PREFLIGHT: std::sync::OnceLock<
    std::sync::Mutex<Vec<(PathBuf, Box<dyn FnOnce(&AtomicBool) + Send>)>>,
> = std::sync::OnceLock::new();

#[cfg(all(test, windows))]
fn run_before_folder_preflight_hook(root: &Path, cancelled: &AtomicBool) {
    let hook = BEFORE_FOLDER_PREFLIGHT
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .ok()
        .and_then(|mut hooks| {
            hooks
                .iter()
                .position(|(expected, _)| expected == root)
                .map(|index| hooks.remove(index).1)
        });
    if let Some(hook) = hook {
        hook(cancelled);
    }
}

pub(super) fn execute(
    token: AttemptToken,
    request: &OperationRequest,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) {
    let mut counters = ProgressCounters {
        total_items: Some(1),
        ..ProgressCounters::default()
    };
    let result = create_zip(token, request, cancelled, controls, emit, &mut counters);
    let (outcome, item_outcome, output, detail, retry) = match result {
        Ok(path) => {
            counters.processed_items = 1;
            (
                TerminalOutcome::Succeeded,
                ItemOutcome::Completed,
                Some(path),
                None,
                RetryPlan::NotAllowed,
            )
        }
        Err(ZipCreateError::Cancelled) => (
            TerminalOutcome::Cancelled,
            ItemOutcome::CancelledBeforeStart,
            None,
            Some("ZIP creation was cancelled before publication".to_owned()),
            RetryPlan::NotAllowed,
        ),
        Err(ZipCreateError::PublishedButUnjournaled(path)) => (
            TerminalOutcome::Unknown,
            ItemOutcome::Unknown,
            Some(path),
            Some("ZIP was published, but confirmation of its output identity or journal receipt is incomplete; inspection is required".to_owned()),
            RetryPlan::RequiresReinspection,
        ),
        Err(ZipCreateError::StageCleanupFailed(detail)) => (
            TerminalOutcome::Partial,
            ItemOutcome::Partial,
            None,
            Some(detail),
            RetryPlan::RequiresReinspection,
        ),
        Err(error) => (
            TerminalOutcome::Failed,
            ItemOutcome::Failed,
            None,
            Some(format!("ZIP creation failed: {error}")),
            RetryPlan::RequiresReinspection,
        ),
    };
    let destination = request.destination.clone();
    let change_hints = destination
        .as_deref()
        .and_then(Path::parent)
        .map(|directory| {
            vec![crate::operations::model::FsChangeHint {
                directory: directory.to_path_buf(),
            }]
        })
        .unwrap_or_default();
    emit(Fact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: counters,
            receipts: vec![ItemReceipt {
                item: ItemId(0),
                outcome: item_outcome,
                detail,
                created_folder_path: None,
                created_archive_path: output,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry,
            change_hints,
        },
    });
}

#[derive(Debug)]
enum ZipCreateError {
    InvalidRequest,
    Cancelled,
    Io(std::io::Error),
    Zip(zip::result::ZipError),
    IdentityChanged,
    Collision,
    Unsupported,
    JournalUnacknowledged,
    PublishedButUnjournaled(PathBuf),
    StageCleanupFailed(String),
}

impl std::fmt::Display for ZipCreateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest => formatter.write_str("invalid captured ZIP request"),
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Zip(error) => write!(formatter, "{error}"),
            Self::IdentityChanged => formatter.write_str("captured filesystem identity changed"),
            Self::Collision => formatter.write_str("output already exists"),
            Self::Unsupported => formatter.write_str("filesystem object is not supported"),
            Self::JournalUnacknowledged => {
                formatter.write_str("ZIP stage journal acknowledgement failed")
            }
            Self::PublishedButUnjournaled(_) => {
                formatter.write_str("ZIP was published but not journaled")
            }
            Self::StageCleanupFailed(detail) => formatter.write_str(detail),
        }
    }
}

impl From<std::io::Error> for ZipCreateError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<zip::result::ZipError> for ZipCreateError {
    fn from(error: zip::result::ZipError) -> Self {
        Self::Zip(error)
    }
}

fn add_known_file_length(total: Option<u64>, length: Option<u64>) -> Option<u64> {
    total?.checked_add(length?)
}

fn create_zip(
    token: AttemptToken,
    request: &OperationRequest,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
    counters: &mut ProgressCounters,
) -> Result<PathBuf, ZipCreateError> {
    if request.kind != OperationKind::CreateZip
        || request.sources.is_empty()
        || request.sources.len() > 1024
        || request.virtual_source.is_some()
    {
        return Err(ZipCreateError::InvalidRequest);
    }
    let destination = request
        .destination
        .as_deref()
        .ok_or(ZipCreateError::InvalidRequest)?;
    let guard = request
        .zip_create_guard
        .as_ref()
        .ok_or(ZipCreateError::InvalidRequest)?;
    let crate::operations::model::ZipOperationGuard::Create(guard) = guard else {
        return Err(ZipCreateError::InvalidRequest);
    };
    if request.sources.len() != guard.sources.len() {
        return Err(ZipCreateError::InvalidRequest);
    }
    let mut common_parent: Option<&Path> = None;
    for (path, source_guard) in request.sources.iter().zip(&guard.sources) {
        let leaf = source_guard.entry_name.as_str();
        if leaf.is_empty()
            || leaf == "."
            || leaf == ".."
            || leaf.contains('/')
            || leaf.contains('\\')
            || !matches!(
                source_guard.source_kind,
                crate::directory::EntryKind::File | crate::directory::EntryKind::Folder
            )
            || path.parent() != destination.parent()
            || common_parent.is_some_and(|parent| parent != path.parent().unwrap_or(parent))
        {
            return Err(ZipCreateError::InvalidRequest);
        }
        common_parent = path.parent();
    }
    if !destination
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        return Err(ZipCreateError::InvalidRequest);
    }
    let parent = destination.parent().ok_or(ZipCreateError::InvalidRequest)?;
    if crate::directory::EntrySourceIdentity::from_path_no_follow(
        parent,
        crate::directory::EntryKind::Folder,
    )
    .is_none_or(|identity| identity.filesystem_identity() != guard.destination_parent_identity)
    {
        return Err(ZipCreateError::IdentityChanged);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(ZipCreateError::Cancelled);
    }

    for (source_path, source_guard) in request.sources.iter().zip(&guard.sources) {
        if crate::directory::EntrySourceIdentity::from_path_no_follow(
            source_path,
            source_guard.source_kind,
        )
        .is_none_or(|identity| identity.filesystem_identity() != source_guard.source_identity)
        {
            return Err(ZipCreateError::IdentityChanged);
        }
    }

    #[cfg(windows)]
    let _parent_route = {
        let route = super::windows::lock_virtual_destination(
            parent,
            Some(&guard.destination_parent_identity),
        )
        .map_err(|_| ZipCreateError::IdentityChanged)?;
        route
    };

    #[cfg(windows)]
    let zip_limits = zip_create_limits(destination);
    let mut pinned_trees = Vec::with_capacity(request.sources.len());
    let mut pinned_files = Vec::with_capacity(request.sources.len());
    let mut remaining_entries = zip_limits.entries;
    let mut remaining_name_bytes = zip_limits.name_bytes;
    for (source_path, source_guard) in request.sources.iter().zip(&guard.sources) {
        if cancelled.load(Ordering::Acquire) {
            return Err(ZipCreateError::Cancelled);
        }
        let prefix_bytes = source_guard.entry_name.len()
            + usize::from(source_guard.source_kind == crate::directory::EntryKind::Folder);
        if remaining_entries == 0 || prefix_bytes > remaining_name_bytes {
            return Err(ZipCreateError::Unsupported);
        }
        remaining_entries -= 1;
        remaining_name_bytes -= prefix_bytes;
        #[cfg(all(test, windows))]
        if source_guard.source_kind == crate::directory::EntryKind::Folder {
            run_before_folder_preflight_hook(source_path, cancelled);
        }
        #[cfg(windows)]
        if source_guard.source_kind == crate::directory::EntryKind::Folder {
            let limits = super::windows::SourceTreeLimits {
                entries: remaining_entries,
                depth: zip_limits.depth.saturating_sub(1),
                relative_name_bytes: remaining_name_bytes,
                record_bytes: usize::MAX,
                emitted_name_prefix_bytes: prefix_bytes,
                zip_utf8_name_bytes: true,
            };
            let tree = super::windows::preflight_pinned_source_tree_for_zip_with_limits(
                source_path,
                cancelled,
                &identity_text(&guard.destination_parent_identity)?,
                &identity_text(&source_guard.source_identity)?,
                limits,
            )
            .map_err(|error| {
                if matches!(
                    error,
                    crate::operations::executor::CopyError::GuardRejected(_)
                ) {
                    ZipCreateError::IdentityChanged
                } else if cancelled.load(Ordering::Acquire) {
                    ZipCreateError::Cancelled
                } else {
                    ZipCreateError::Unsupported
                }
            })?;
            if tree.root_identity != identity_text(&source_guard.source_identity)? {
                return Err(ZipCreateError::IdentityChanged);
            }
            remaining_entries = remaining_entries.saturating_sub(tree.entries.len());
            remaining_name_bytes = remaining_name_bytes.saturating_sub(
                tree.entries
                    .iter()
                    .map(|entry| {
                        prefix_bytes
                            + entry
                                .relative_components
                                .iter()
                                .map(|part| String::from_utf16(part).map(|text| text.len()).unwrap_or(0))
                                .sum::<usize>()
                            + entry.relative_components.len().saturating_sub(1)
                            + usize::from(entry.node_kind == crate::operations::journal::JournalDirectoryNodeKind::Directory)
                    })
                    .sum::<usize>(),
            );
            pinned_trees.push(Some(tree));
            pinned_files.push(None);
        } else {
            let file = super::windows::open_pinned_file_with_access(source_path, false)
                .map_err(|_| ZipCreateError::IdentityChanged)?;
            verify_source_handle(source_path, &file, &source_guard.source_identity)?;
            pinned_files.push(Some(file));
            pinned_trees.push(None);
        }
    }
    #[cfg(not(windows))]
    let mut pinned_tree: Option<()> = None;

    #[cfg(windows)]
    {
        let mut total_bytes = Some(0_u64);
        for (source_index, source_guard) in guard.sources.iter().enumerate() {
            if source_guard.source_kind == crate::directory::EntryKind::File {
                let file = pinned_files[source_index]
                    .as_ref()
                    .ok_or(ZipCreateError::InvalidRequest)?;
                total_bytes = add_known_file_length(
                    total_bytes,
                    file.metadata().ok().map(|metadata| metadata.len()),
                );
            } else {
                let tree = pinned_trees[source_index]
                    .as_ref()
                    .ok_or(ZipCreateError::InvalidRequest)?;
                for entry in &tree.entries {
                    if entry.node_kind == crate::operations::journal::JournalDirectoryNodeKind::File
                    {
                        let Some(Some(super::windows::PinnedSourceHandle::File(file))) =
                            tree._handles.get(entry.handle_index)
                        else {
                            return Err(ZipCreateError::InvalidRequest);
                        };
                        total_bytes = add_known_file_length(
                            total_bytes,
                            file.metadata().ok().map(|metadata| metadata.len()),
                        );
                    }
                }
            }
        }
        counters.total_bytes = total_bytes;
    }

    let stage_path = unique_stage_path(parent, token)?;
    let mut ledger = super::super::journal::JournalZipCreate {
        sources: request
            .sources
            .iter()
            .zip(&guard.sources)
            .map(
                |(path, source)| super::super::journal::JournalZipCreateSource {
                    source: super::super::journal::JournalPath::from_path(path),
                    source_identity: format!("{:?}", source.source_identity),
                },
            )
            .collect(),
        output: super::super::journal::JournalPath::from_path(destination),
        stage: super::super::journal::JournalPath::from_path(&stage_path),
        parent_identity: format!("{:?}", guard.destination_parent_identity),
        stage_identity: None,
        output_identity: None,
        phase: super::super::journal::JournalZipCreatePhase::StagePlanned,
    };
    journal_ledger(token, &ledger, controls, emit)?;
    if cancelled.load(Ordering::Acquire) {
        ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
        journal_ledger(token, &ledger, controls, emit)?;
        return Err(ZipCreateError::Cancelled);
    }
    let stage_file = create_stage_file(&stage_path)?;
    let mut stage_guard = StageGuard::new(stage_path, stage_file);
    ledger.stage_identity = match stage_identity(stage_guard.file.as_ref().expect("stage file")) {
        Ok(identity) => Some(identity),
        Err(error) => {
            if let Err(cleanup_error) = stage_guard.cleanup() {
                return Err(ZipCreateError::StageCleanupFailed(format!(
                    "owned ZIP stage cleanup failed at {} (identity unavailable): {cleanup_error}",
                    stage_guard.path.display()
                )));
            }
            ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
            journal_ledger(token, &ledger, controls, emit)?;
            return Err(error);
        }
    };
    ledger.phase = super::super::journal::JournalZipCreatePhase::StageOwned;
    if let Err(error) = journal_ledger(token, &ledger, controls, emit) {
        if let Err(cleanup_error) = stage_guard.cleanup() {
            return Err(ZipCreateError::StageCleanupFailed(format!(
                "owned ZIP stage cleanup failed at {} (identity {}): {cleanup_error}",
                stage_guard.path.display(),
                ledger.stage_identity.as_deref().unwrap_or("unknown")
            )));
        }
        ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
        let _ = journal_ledger(token, &ledger, controls, emit);
        return Err(error);
    }
    let owned_stage_identity = ledger.stage_identity.clone().unwrap_or_default();
    let result = (|| -> Result<PathBuf, ZipCreateError> {
        let stage_file = stage_guard
            .file
            .as_ref()
            .expect("stage handle is present")
            .try_clone()?;
        let mut archive = zip::ZipWriter::new(stage_file);
        let mut progress_sequence = 0_u64;
        let mut buffer = vec![0_u8; CHUNK_BYTES];
        #[cfg(windows)]
        for (source_index, (source_path, source_guard)) in
            request.sources.iter().zip(&guard.sources).enumerate()
        {
            if cancelled.load(Ordering::Acquire) {
                return Err(ZipCreateError::Cancelled);
            }
            let leaf = source_guard.entry_name.as_str();
            if source_guard.source_kind == crate::directory::EntryKind::File {
                // 圧縮後サイズは元サイズを超えることがあり、stream 開始後の予約では間に合わない。
                archive.start_file(
                    leaf,
                    zip::write::SimpleFileOptions::default().large_file(true),
                )?;
                let input = pinned_files[source_index]
                    .as_mut()
                    .expect("file root was pinned before staging");
                input.seek(SeekFrom::Start(0))?;
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(ZipCreateError::Cancelled);
                    }
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    archive.write_all(&buffer[..read])?;
                    counters.processed_bytes = counters.processed_bytes.saturating_add(read as u64);
                    progress_sequence = progress_sequence.saturating_add(1);
                    emit(Fact::Progress {
                        token,
                        update: crate::operations::model::ProgressUpdate {
                            sequence: progress_sequence,
                            counters: *counters,
                            current_item: Some(ItemId(0)),
                        },
                    });
                }
                verify_source_handle(source_path, input, &source_guard.source_identity)?;
            } else {
                archive
                    .add_directory(format!("{leaf}/"), zip::write::SimpleFileOptions::default())?;
                progress_sequence = progress_sequence.saturating_add(1);
                emit(Fact::Progress {
                    token,
                    update: crate::operations::model::ProgressUpdate {
                        sequence: progress_sequence,
                        counters: *counters,
                        current_item: Some(ItemId(0)),
                    },
                });
                let tree = pinned_trees[source_index]
                    .as_mut()
                    .expect("folder root was pinned before staging");
                for entry in &tree.entries {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(ZipCreateError::Cancelled);
                    }
                    let mut entry_path = leaf.to_owned();
                    for component in &entry.relative_components {
                        let part = String::from_utf16(component)
                            .map_err(|_| ZipCreateError::Unsupported)?;
                        if part.is_empty()
                            || part == "."
                            || part == ".."
                            || part.contains('/')
                            || part.contains('\\')
                        {
                            return Err(ZipCreateError::Unsupported);
                        }
                        entry_path.push('/');
                        entry_path.push_str(&part);
                    }
                    match entry.node_kind {
                        crate::operations::journal::JournalDirectoryNodeKind::Directory => {
                            archive.add_directory(
                                format!("{entry_path}/"),
                                zip::write::SimpleFileOptions::default(),
                            )?;
                            progress_sequence = progress_sequence.saturating_add(1);
                            emit(Fact::Progress {
                                token,
                                update: crate::operations::model::ProgressUpdate {
                                    sequence: progress_sequence,
                                    counters: *counters,
                                    current_item: Some(ItemId(0)),
                                },
                            });
                        }
                        crate::operations::journal::JournalDirectoryNodeKind::File => {
                            archive.start_file(
                                &entry_path,
                                zip::write::SimpleFileOptions::default().large_file(true),
                            )?;
                            let Some(Some(super::windows::PinnedSourceHandle::File(input))) =
                                tree._handles.get_mut(entry.handle_index)
                            else {
                                return Err(ZipCreateError::Unsupported);
                            };
                            input.seek(SeekFrom::Start(0))?;
                            loop {
                                if cancelled.load(Ordering::Acquire) {
                                    return Err(ZipCreateError::Cancelled);
                                }
                                let read = input.read(&mut buffer)?;
                                if read == 0 {
                                    break;
                                }
                                archive.write_all(&buffer[..read])?;
                                counters.processed_bytes =
                                    counters.processed_bytes.saturating_add(read as u64);
                                progress_sequence = progress_sequence.saturating_add(1);
                                emit(Fact::Progress {
                                    token,
                                    update: crate::operations::model::ProgressUpdate {
                                        sequence: progress_sequence,
                                        counters: *counters,
                                        current_item: Some(ItemId(0)),
                                    },
                                });
                            }
                        }
                        crate::operations::journal::JournalDirectoryNodeKind::Unknown => {
                            return Err(ZipCreateError::Unsupported);
                        }
                    }
                }
            }
        }
        #[cfg(not(windows))]
        return Err(ZipCreateError::Unsupported);
        let mut stage_file: File = archive.finish()?;
        stage_file.flush()?;
        stage_file.sync_all()?;
        if cancelled.load(Ordering::Acquire) {
            stage_guard.cleanup()?;
            ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
            journal_ledger(token, &ledger, controls, emit)?;
            return Err(ZipCreateError::Cancelled);
        }
        verify_parent(parent, &guard.destination_parent_identity)?;
        ledger.phase = super::super::journal::JournalZipCreatePhase::PublishIntent;
        journal_ledger(token, &ledger, controls, emit)?;
        if cancelled.load(Ordering::Acquire) {
            stage_guard.cleanup()?;
            ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
            journal_ledger(token, &ledger, controls, emit)?;
            return Err(ZipCreateError::Cancelled);
        }
        let output = destination.to_path_buf();
        let publish_result = publish_stage(&stage_file, &output);
        match publish_result {
            Ok(()) => {
                stage_guard.disarm();
                // exFAT では rename 時に file identity が変わる場合があるため、公開後に retained handle から読む。
                ledger.output_identity = Some(
                    output_identity_after_publish(&stage_file, &output)
                        .map_err(|_| ZipCreateError::PublishedButUnjournaled(output.clone()))?,
                );
                ledger.phase = super::super::journal::JournalZipCreatePhase::Published;
                journal_ledger(token, &ledger, controls, emit)
                    .map_err(|_| ZipCreateError::PublishedButUnjournaled(output.clone()))?;
                Ok(output)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                stage_guard.cleanup()?;
                ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
                journal_ledger(token, &ledger, controls, emit)?;
                Err(ZipCreateError::Collision)
            }
            Err(error) => {
                stage_guard.cleanup()?;
                ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
                journal_ledger(token, &ledger, controls, emit)?;
                Err(ZipCreateError::Io(error))
            }
        }
    })();
    if result.is_err() && stage_guard.owned {
        if let Err(cleanup_error) = stage_guard.cleanup() {
            return Err(ZipCreateError::StageCleanupFailed(format!(
                "owned ZIP stage cleanup failed at {} (identity {}): {cleanup_error}",
                stage_guard.path.display(),
                owned_stage_identity
            )));
        }
        ledger.phase = super::super::journal::JournalZipCreatePhase::StageDiscarded;
        if let Err(journal_error) = journal_ledger(token, &ledger, controls, emit) {
            return Err(journal_error);
        }
    }
    result
}

fn open_source(path: &Path) -> Result<File, ZipCreateError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // OPEN_REPARSE_POINT pins the directory entry itself; reject the handle if it is a link.
        options.share_mode(0x0000_0001).custom_flags(0x0020_0000);
    }
    options.open(path).map_err(Into::into)
}

fn verify_source_handle(
    path: &Path,
    file: &File,
    expected: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<(), ZipCreateError> {
    let current = crate::directory::EntrySourceIdentity::from_path_no_follow(
        path,
        crate::directory::EntryKind::File,
    )
    .ok_or(ZipCreateError::IdentityChanged)?;
    if current.filesystem_identity() != *expected {
        return Err(ZipCreateError::IdentityChanged);
    }
    #[cfg(windows)]
    {
        let actual =
            super::windows::regular_file_identity(file).map_err(|_| ZipCreateError::Unsupported)?;
        let expected_parts = expected
            .windows_parts()
            .ok_or(ZipCreateError::IdentityChanged)?;
        if expected_parts
            != (
                actual.volume,
                ((actual.index_high as u64) << 32) | actual.index_low as u64,
            )
        {
            return Err(ZipCreateError::IdentityChanged);
        }
    }
    #[cfg(not(windows))]
    let _ = file;
    Ok(())
}

fn verify_parent(
    parent: &Path,
    expected: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<(), ZipCreateError> {
    let current = crate::directory::EntrySourceIdentity::from_path_no_follow(
        parent,
        crate::directory::EntryKind::Folder,
    )
    .ok_or(ZipCreateError::IdentityChanged)?;
    if current.filesystem_identity() == *expected {
        Ok(())
    } else {
        Err(ZipCreateError::IdentityChanged)
    }
}

fn journal_ledger(
    token: AttemptToken,
    ledger: &super::super::journal::JournalZipCreate,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) -> Result<(), ZipCreateError> {
    emit(Fact::ZipCreateLedgerIntent {
        token,
        ledger: ledger.clone(),
    });
    loop {
        match controls.recv() {
            Ok(WorkerControl::ZipCreateLedgerJournaledExact {
                token: ack,
                ledger: saved,
            }) if ack == token && saved == *ledger => return Ok(()),
            Ok(WorkerControl::ZipCreateLedgerJournalFailedExact {
                token: ack,
                ledger: failed,
            }) if ack == token && failed == *ledger => {
                return Err(ZipCreateError::JournalUnacknowledged);
            }
            // Runtime forwards cancellation separately from the durable stage receipt.
            // Consume it here so the exact ledger acknowledgment remains authoritative.
            Ok(WorkerControl::Cancel)
                if ledger.phase == super::super::journal::JournalZipCreatePhase::StageDiscarded =>
            {
                continue;
            }
            Ok(WorkerControl::Cancel)
                if ledger.phase == super::super::journal::JournalZipCreatePhase::StageOwned =>
            {
                // Cancel may win after stage creation but before its ownership fact is durable.
                // Continue only so the worker observes cancellation and journals exact cleanup.
                return Ok(());
            }
            _ => return Err(ZipCreateError::JournalUnacknowledged),
        }
    }
}

fn identity_text(
    identity: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<String, ZipCreateError> {
    let (volume, index) = identity
        .windows_parts()
        .ok_or(ZipCreateError::Unsupported)?;
    Ok(format!(
        "{volume}:{:08x}{:08x}",
        (index >> 32) as u32,
        index as u32
    ))
}

fn stage_identity(file: &File) -> Result<String, ZipCreateError> {
    #[cfg(windows)]
    {
        let id =
            super::windows::regular_file_identity(file).map_err(|_| ZipCreateError::Unsupported)?;
        return Ok(format!(
            "{:08x}:{:08x}{:08x}",
            id.volume, id.index_high, id.index_low
        ));
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        Err(ZipCreateError::Unsupported)
    }
}

#[cfg(test)]
static OUTPUT_IDENTITY_FAILURE_FOR_TEST: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

fn output_identity_after_publish(file: &File, _output: &Path) -> Result<String, ZipCreateError> {
    #[cfg(test)]
    {
        let mut failure_for = OUTPUT_IDENTITY_FAILURE_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap();
        if failure_for.as_deref() == Some(_output) {
            *failure_for = None;
            return Err(ZipCreateError::Unsupported);
        }
    }
    stage_identity(file)
}

fn unique_stage_path(parent: &Path, token: AttemptToken) -> Result<PathBuf, ZipCreateError> {
    for _ in 0..16 {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| ZipCreateError::Unsupported)?;
        let mut leaf = format!(
            ".gpui-filer-zip-{}-{}-",
            token.job_id().value(),
            token.attempt_id().value()
        );
        for byte in nonce {
            use std::fmt::Write as _;
            let _ = write!(&mut leaf, "{byte:02x}");
        }
        leaf.push_str(".stage");
        let path = parent.join(leaf);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(ZipCreateError::Unsupported)
}

fn create_stage_file(path: &Path) -> Result<File, ZipCreateError> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .access_mode(0x4000_0000 | 0x0001_0000)
            // stage の内容と名前は publish までこのhandleだけが変更できるようにする。
            .share_mode(0x0000_0001);
    }
    options.open(path).map_err(Into::into)
}

fn publish_stage(file: &File, destination: &Path) -> Result<(), std::io::Error> {
    #[cfg(windows)]
    {
        super::windows::publish_no_replace_handle(super::windows::raw(file), destination).map_err(
            |error| match error {
                super::CopyError::DestinationExists => {
                    std::io::Error::new(std::io::ErrorKind::AlreadyExists, "ZIP destination exists")
                }
                other => std::io::Error::other(format!("{other:?}")),
            },
        )
    }
    #[cfg(not(windows))]
    {
        let source = file;
        let _ = (source, destination);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "ZIP publication requires the Windows handle adapter",
        ))
    }
}

struct StageGuard {
    path: PathBuf,
    file: Option<File>,
    owned: bool,
}
impl StageGuard {
    fn new(path: PathBuf, file: File) -> Self {
        Self {
            path,
            file: Some(file),
            owned: true,
        }
    }
    fn disarm(&mut self) {
        self.owned = false;
    }
    fn cleanup(&mut self) -> Result<(), ZipCreateError> {
        if !self.owned {
            return Ok(());
        }
        if let Some(file) = &self.file {
            #[cfg(windows)]
            super::windows::cleanup_owned(file).map_err(|error| {
                ZipCreateError::StageCleanupFailed(format!(
                    "owned ZIP stage cleanup failed: {error:?}"
                ))
            })?;
            #[cfg(not(windows))]
            std::fs::remove_file(&self.path)?;
        }
        self.owned = false;
        Ok(())
    }
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        if self.owned {
            if let Some(file) = &self.file {
                #[cfg(windows)]
                let _ = super::windows::cleanup_owned(file);
                #[cfg(not(windows))]
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::operations::model::OperationRequest;
    use std::sync::Arc;
    use std::sync::mpsc;

    #[test]
    fn aggregate_file_lengths_preserves_unknown_and_overflow_as_unknown() {
        assert_eq!(super::add_known_file_length(Some(0), Some(4)), Some(4));
        assert_eq!(super::add_known_file_length(Some(4), Some(6)), Some(10));
        assert_eq!(super::add_known_file_length(Some(4), None), None);
        assert_eq!(super::add_known_file_length(None, Some(6)), None);
        assert_eq!(super::add_known_file_length(Some(u64::MAX), Some(1)), None);
    }

    fn run_zip(
        request: OperationRequest,
        cancel_on_progress: bool,
        collision_on_publish_intent: bool,
        cancel_after_publish: bool,
    ) -> (
        TerminalReport,
        Vec<super::super::super::journal::JournalZipCreate>,
    ) {
        let token = crate::operations::model::OperationService::new(1)
            .submit(request.clone())
            .unwrap();
        let collision_path = request.destination.clone().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let (control_tx, control_rx) = mpsc::channel();
        let (fact_tx, fact_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute(
                token,
                &request,
                &worker_cancelled,
                &control_rx,
                &mut |fact| {
                    let _ = fact_tx.send(fact);
                },
            );
        });
        let mut ledgers = Vec::new();
        let mut terminal = None;
        while let Ok(fact) = fact_rx.recv() {
            match fact {
                Fact::ZipCreateLedgerIntent { token, ledger } => {
                    if collision_on_publish_intent
                        && ledger.phase
                            == super::super::super::journal::JournalZipCreatePhase::PublishIntent
                    {
                        std::fs::write(&collision_path, b"sentinel").unwrap();
                    }
                    ledgers.push(ledger.clone());
                    if cancel_after_publish
                        && ledger.phase
                            == super::super::super::journal::JournalZipCreatePhase::Published
                    {
                        // Runtime rejects the durable receipt ACK after cancellation and Shell
                        // forwards Cancel; the worker must report the already-published file.
                        cancelled.store(true, Ordering::Release);
                        control_tx.send(WorkerControl::Cancel).unwrap();
                    } else {
                        control_tx
                            .send(WorkerControl::ZipCreateLedgerJournaledExact { token, ledger })
                            .unwrap();
                    }
                }
                Fact::Progress { .. } if cancel_on_progress => {
                    cancelled.store(true, Ordering::Release);
                    control_tx.send(WorkerControl::Cancel).unwrap();
                }
                Fact::Terminal { report, .. } => {
                    terminal = Some(report);
                    break;
                }
                _ => {}
            }
        }
        worker.join().unwrap();
        (terminal.expect("terminal report"), ledgers)
    }

    fn run_zip_collect_progress(
        request: OperationRequest,
    ) -> (
        TerminalReport,
        Vec<crate::operations::model::ProgressUpdate>,
        Vec<super::super::super::journal::JournalZipCreate>,
    ) {
        let token = crate::operations::model::OperationService::new(1)
            .submit(request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let (fact_tx, fact_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute(
                token,
                &request,
                &AtomicBool::new(false),
                &control_rx,
                &mut |fact| {
                    let _ = fact_tx.send(fact);
                },
            );
        });
        let mut progress = Vec::new();
        let mut ledgers = Vec::new();
        let mut terminal = None;
        while let Ok(fact) = fact_rx.recv() {
            match fact {
                Fact::ZipCreateLedgerIntent { token, ledger } => {
                    ledgers.push(ledger.clone());
                    control_tx
                        .send(WorkerControl::ZipCreateLedgerJournaledExact { token, ledger })
                        .unwrap();
                }
                Fact::Progress { update, .. } => progress.push(update),
                Fact::Terminal { report, .. } => {
                    terminal = Some(report);
                    break;
                }
                _ => {}
            }
        }
        worker.join().unwrap();
        (terminal.expect("terminal report"), progress, ledgers)
    }

    fn assert_exact_progress_totals(
        report: &crate::operations::model::TerminalReport,
        progress: &[crate::operations::model::ProgressUpdate],
        expected: u64,
    ) {
        assert_eq!(report.final_counters.total_bytes, Some(expected));
        assert_eq!(report.final_counters.processed_bytes, expected);
        assert!(!progress.is_empty());
        for update in progress {
            assert_eq!(update.counters.total_bytes, Some(expected));
            assert!(update.counters.processed_bytes <= expected);
        }
        assert_eq!(progress.last().unwrap().counters.processed_bytes, expected);
    }

    #[cfg(windows)]
    #[test]
    fn cancel_before_runtime_commits_stage_owned_persists_exact_cleanup() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let source = root.path().join("cancel-source.txt");
        let second_source = root.path().join("cancel-source-two.txt");
        std::fs::write(&source, b"cancel before owned ack").unwrap();
        std::fs::write(&second_source, b"second captured source").unwrap();
        let output = root.path().join("cancel-output.zip");
        let first_guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let second_guard = crate::operations::model::test_zip_create_guard(&second_source, &output);
        let request = OperationRequest::create_zip_many(
            vec![source.clone(), second_source.clone()],
            output.clone(),
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity,
                sources: vec![
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: second_guard.source_identity,
                        source_kind: second_guard.source_kind,
                        entry_name: second_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        let journal_path = root.path().join("journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured ZIP request");
        };
        let controls = start.controls;
        let cancellation = start.cancellation;
        let (fact_tx, fact_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute(
                token,
                &request,
                cancellation.as_ref(),
                &controls,
                &mut |fact| {
                    fact_tx.send(fact).unwrap();
                },
            );
        });

        let mut planned = None;
        let mut saw_discarded = false;
        let mut discarded_ledger = None;
        let mut terminal_report = None;
        while let Ok(fact) = fact_rx.recv() {
            match fact {
                Fact::ZipCreateLedgerIntent {
                    token: fact_token,
                    ledger,
                } => {
                    match ledger.phase {
                        crate::operations::journal::JournalZipCreatePhase::StagePlanned => {
                            runtime.commit_fact(Fact::ZipCreateLedgerIntent {
                                token: fact_token,
                                ledger: ledger.clone(),
                            });
                            planned = Some(ledger.clone());
                            let record = runtime.journal_attempt(token).unwrap();
                            writer
                                .upsert_with_reply(record)
                                .recv_blocking()
                                .unwrap()
                                .unwrap();
                            assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
                        }
                        crate::operations::journal::JournalZipCreatePhase::StageOwned => {
                            // Cancel wins before the Runtime accepts the owned-stage fact.
                            assert!(runtime.cancel(token).is_none());
                            let cancelled = runtime.journal_attempt(token).unwrap();
                            assert_eq!(
                                cancelled.state,
                                crate::operations::journal::JournalState::CancelRequested
                            );
                            assert_eq!(
                                cancelled.zip_create.as_ref().unwrap().phase,
                                crate::operations::journal::JournalZipCreatePhase::StagePlanned
                            );
                            assert_eq!(cancelled.zip_create.as_ref().unwrap().stage_identity, None);
                            writer
                                .upsert_with_reply(cancelled)
                                .recv_blocking()
                                .unwrap()
                                .unwrap();
                            let persisted_cancel: serde_json::Value =
                                serde_json::from_slice(&std::fs::read(&journal_path).unwrap())
                                    .unwrap();
                            assert_eq!(
                                persisted_cancel["attempts"][0]["state"],
                                "cancel-requested"
                            );
                            assert_eq!(
                                persisted_cancel["attempts"][0]["zip_create"]["phase"],
                                "stage-planned"
                            );
                            runtime.commit_fact(Fact::ZipCreateLedgerIntent {
                                token: fact_token,
                                ledger,
                            });
                        }
                        crate::operations::journal::JournalZipCreatePhase::StageDiscarded => {
                            runtime.commit_fact(Fact::ZipCreateLedgerIntent {
                                token: fact_token,
                                ledger: ledger.clone(),
                            });
                            let record = runtime.journal_attempt(token).unwrap();
                            writer
                                .upsert_with_reply(record)
                                .recv_blocking()
                                .unwrap()
                                .unwrap();
                            assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
                            saw_discarded = true;
                            discarded_ledger = Some(ledger);
                        }
                        phase => {
                            panic!("unexpected ZIP ledger phase during cancellation: {phase:?}")
                        }
                    }
                }
                Fact::Terminal {
                    token: fact_token,
                    report,
                } => {
                    assert_eq!(fact_token, token);
                    assert_eq!(report.outcome, TerminalOutcome::Cancelled);
                    let terminal = runtime
                        .commit_fact(Fact::Terminal {
                            token,
                            report: report.clone(),
                        })
                        .expect("runtime accepts cancelled terminal report");
                    let record = runtime.journal_attempt(token).unwrap();
                    writer
                        .upsert_with_reply(record)
                        .recv_blocking()
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        terminal.event,
                        crate::operations::model::ServiceEvent::TerminalCommitted {
                            token,
                            report: report.clone()
                        }
                    );
                    terminal_report = Some(report);
                    break;
                }
                _ => {}
            }
        }
        worker.join().unwrap();
        let report = terminal_report.expect("worker reports terminal cancellation");
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(report.receipts[0].item, ItemId(0));
        assert_eq!(
            report.receipts[0].outcome,
            ItemOutcome::CancelledBeforeStart
        );
        assert!(saw_discarded);
        assert!(!output.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"cancel before owned ack");
        assert_eq!(
            std::fs::read(&second_source).unwrap(),
            b"second captured source"
        );
        use std::os::windows::ffi::OsStringExt;
        let stage = PathBuf::from(std::ffi::OsString::from_wide(
            &planned.as_ref().expect("planned stage ledger").stage.units,
        ));
        assert!(!stage.exists(), "the worker removed its owned stage");
        assert!(
            runtime.start_next().is_none(),
            "the terminal attempt releases the runtime lane"
        );
        drop(writer);
        let persisted = std::fs::read(&journal_path).unwrap();
        let persisted_json: serde_json::Value = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(persisted_json["attempts"][0]["state"], "terminal");
        assert_eq!(
            persisted_json["attempts"][0]["terminal_outcome"],
            "cancelled"
        );
        assert_eq!(
            persisted_json["attempts"][0]["zip_create"]["phase"],
            "stage-discarded"
        );
        assert_eq!(
            persisted_json["attempts"][0]["zip_create"]["sources"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            persisted_json["attempts"][0]["zip_create"]["stage_identity"]
                .as_str()
                .is_some_and(|identity| !identity.is_empty())
        );
        assert_eq!(
            persisted_json["attempts"][0]["zip_create"]["stage_identity"],
            discarded_ledger.unwrap().stage_identity.unwrap()
        );
        assert_eq!(
            persisted_json["attempts"][0]["zip_create"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|source| source["source"]["units"].clone())
                .collect::<Vec<_>>(),
            [source.clone(), second_source.clone()]
                .iter()
                .map(|source| {
                    serde_json::to_value(
                        crate::operations::journal::JournalPath::from_path(source).units,
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
        );
        let reopened = crate::operations::journal::JournalStore::open(journal_path).unwrap();
        assert_eq!(reopened.attempt_keys().len(), 1);
        let attempt = reopened
            .recoveries()
            .into_iter()
            .find_map(|item| match item {
                crate::operations::journal::RecoveryItem::RecoveryRequired(attempt) => {
                    Some(attempt)
                }
                _ => None,
            });
        assert!(attempt.is_none(), "cancelled ZIP is not left for recovery");
    }

    fn assert_file_local_headers_reserve_zip64(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut file_count = 0;
        for index in 0..archive.len() {
            let entry = archive.by_index(index).unwrap();
            let is_dir = entry.is_dir();
            let offset = usize::try_from(entry.header_start()).unwrap();
            assert_eq!(&bytes[offset..offset + 4], b"PK\x03\x04");
            let compressed_32 =
                u32::from_le_bytes(bytes[offset + 18..offset + 22].try_into().unwrap());
            let uncompressed_32 =
                u32::from_le_bytes(bytes[offset + 22..offset + 26].try_into().unwrap());
            let name_len = usize::from(u16::from_le_bytes(
                bytes[offset + 26..offset + 28].try_into().unwrap(),
            ));
            let extra_len = usize::from(u16::from_le_bytes(
                bytes[offset + 28..offset + 30].try_into().unwrap(),
            ));
            let extra = &bytes[offset + 30 + name_len..offset + 30 + name_len + extra_len];
            let mut cursor = 0;
            let mut found_zip64 = false;
            while cursor + 4 <= extra.len() {
                let id = u16::from_le_bytes(extra[cursor..cursor + 2].try_into().unwrap());
                let length = usize::from(u16::from_le_bytes(
                    extra[cursor + 2..cursor + 4].try_into().unwrap(),
                ));
                let payload = &extra[cursor + 4..cursor + 4 + length];
                if id == 0x0001 {
                    assert!(length >= 16);
                    assert_eq!(
                        u64::from_le_bytes(payload[..8].try_into().unwrap()),
                        entry.size()
                    );
                    assert_eq!(
                        u64::from_le_bytes(payload[8..16].try_into().unwrap()),
                        entry.compressed_size()
                    );
                    found_zip64 = true;
                    break;
                }
                cursor += 4 + length;
            }
            if is_dir {
                assert_eq!(compressed_32, 0, "directory compressed size stays classic");
                assert_eq!(uncompressed_32, 0, "directory size stays classic");
                assert!(!found_zip64, "directory unexpectedly has ZIP64 size data");
            } else {
                file_count += 1;
                assert_eq!(compressed_32, u32::MAX);
                assert_eq!(uncompressed_32, u32::MAX);
                assert!(
                    found_zip64,
                    "{} lacks local ZIP64 size reservation",
                    entry.name()
                );
            }
        }
        assert!(file_count > 0);
    }

    fn run_builtin_extract(request: OperationRequest) -> TerminalReport {
        let token = crate::operations::model::OperationService::default()
            .submit(request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let mut terminal = None;
        super::super::zip_extract::execute(
            token,
            &request,
            &AtomicBool::new(false),
            &control_rx,
            &mut |fact| match fact {
                Fact::ZipExtractLedgerIntent {
                    token,
                    item,
                    ledger,
                } => control_tx
                    .send(WorkerControl::ZipExtractLedgerJournaledExact {
                        token,
                        item,
                        ledger,
                    })
                    .unwrap(),
                Fact::Terminal { report, .. } => terminal = Some(report),
                _ => {}
            },
        );
        terminal.expect("extract worker terminal report")
    }

    #[test]
    fn creates_unicode_zip_with_one_file_and_round_trips_empty_files() {
        for (name, contents) in [
            ("資料.txt", b"hello zip".as_slice()),
            ("empty", b"".as_slice()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join(name);
            std::fs::write(&source, contents).unwrap();
            let output = root.path().join("bundle.zip");
            let report = run_zip(
                OperationRequest::create_zip(
                    source.clone(),
                    output.clone(),
                    crate::operations::model::test_zip_create_guard(&source, &output),
                )
                .unwrap(),
                false,
                false,
                false,
            )
            .0;
            assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
            assert_file_local_headers_reserve_zip64(&output);
            assert_eq!(
                report.receipts[0].created_archive_path.as_deref(),
                Some(output.as_path())
            );
            let mut archive = zip::ZipArchive::new(File::open(output).unwrap()).unwrap();
            assert_eq!(archive.len(), 1);
            let mut entry = archive.by_index(0).unwrap();
            assert_eq!(entry.name(), name);
            let mut actual = Vec::new();
            entry.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, contents);
        }
    }

    #[test]
    fn created_single_and_folder_archives_round_trip_through_builtin_extract_worker() {
        let parent = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        for folder_source in [false, true] {
            let source = if folder_source {
                let root = parent.path().join("bundle-root");
                std::fs::create_dir_all(root.join("nested/empty")).unwrap();
                std::fs::write(root.join("nested/資料.txt"), b"nested bytes").unwrap();
                root
            } else {
                let path = parent.path().join("単独.txt");
                std::fs::write(&path, b"single bytes").unwrap();
                path
            };
            let archive_path = parent.path().join(if folder_source {
                "folder.zip"
            } else {
                "single.zip"
            });
            let guard = if folder_source {
                crate::operations::model::test_zip_create_guard_for_kind(
                    &source,
                    &archive_path,
                    crate::directory::EntryKind::Folder,
                )
            } else {
                crate::operations::model::test_zip_create_guard(&source, &archive_path)
            };
            let create =
                OperationRequest::create_zip(source.clone(), archive_path.clone(), guard).unwrap();
            let report = run_zip(create, false, false, false).0;
            assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
            assert!(source.exists(), "source must remain intact");

            let extracted = parent
                .path()
                .join(if folder_source { "folder" } else { "single" });
            let extract_guard =
                crate::operations::model::test_zip_create_guard(&archive_path, &extracted);
            let request = OperationRequest::extract_zip(
                archive_path.clone(),
                extracted.clone(),
                extract_guard,
            )
            .unwrap();
            let extract_report = run_builtin_extract(request);
            assert_eq!(
                extract_report.outcome,
                TerminalOutcome::Succeeded,
                "{extract_report:?}"
            );
            assert!(archive_path.is_file(), "source archive must remain intact");
            if folder_source {
                assert_eq!(
                    std::fs::read(extracted.join("bundle-root/nested/資料.txt")).unwrap(),
                    b"nested bytes"
                );
                assert!(extracted.join("bundle-root/nested/empty").is_dir());
            } else {
                assert_eq!(
                    std::fs::read(extracted.join("単独.txt")).unwrap(),
                    b"single bytes"
                );
            }
        }
    }

    #[test]
    fn creates_folder_zip_with_root_empty_directories_and_nested_files() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("archive.source");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(source.join("empty")).unwrap();
        std::fs::create_dir_all(source.join("nested/deeper")).unwrap();
        std::fs::write(source.join("nested/deeper/data.txt"), b"folder zip").unwrap();
        let output = parent.path().join("archive.source.zip");
        let guard = crate::operations::model::test_zip_create_guard_for_kind(
            &source,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let request = OperationRequest::create_zip(source, output.clone(), guard).unwrap();
        let (report, ledgers) = run_zip(request, false, false, false);
        assert_eq!(
            report.outcome,
            TerminalOutcome::Succeeded,
            "{report:?}; ledgers={ledgers:?}"
        );
        let stage_identity_fact = ledgers
            .iter()
            .find(|ledger| {
                ledger.phase == super::super::super::journal::JournalZipCreatePhase::StageOwned
            })
            .and_then(|ledger| ledger.stage_identity.as_deref())
            .expect("owned stage identity");
        let output_identity = ledgers
            .iter()
            .find(|ledger| {
                ledger.phase == super::super::super::journal::JournalZipCreatePhase::Published
            })
            .and_then(|ledger| ledger.output_identity.as_deref())
            .expect("published output identity");
        assert!(!stage_identity_fact.is_empty());
        assert!(!output_identity.is_empty());
        assert_eq!(
            output_identity,
            stage_identity(&File::open(&output).unwrap()).unwrap(),
            "published identity must match the object reached through the output path"
        );
        assert_file_local_headers_reserve_zip64(&output);
        let mut archive = zip::ZipArchive::new(File::open(output).unwrap()).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        assert!(names.contains(&"archive.source/".to_owned()));
        assert!(names.contains(&"archive.source/empty/".to_owned()));
        assert!(names.contains(&"archive.source/nested/deeper/".to_owned()));
        let mut file = archive
            .by_name("archive.source/nested/deeper/data.txt")
            .unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"folder zip");

        let cancel_output = parent.path().join("archive.source-cancelled.zip");
        let cancel_request = OperationRequest::create_zip(
            parent.path().join("archive.source"),
            cancel_output.clone(),
            crate::operations::model::test_zip_create_guard_for_kind(
                &parent.path().join("archive.source"),
                &cancel_output,
                crate::directory::EntryKind::Folder,
            ),
        )
        .unwrap();
        let (cancel_report, ledgers) = run_zip(cancel_request, true, false, false);
        assert_eq!(
            cancel_report.outcome,
            TerminalOutcome::Cancelled,
            "{cancel_report:?}"
        );
        assert!(!cancel_output.exists());
        assert!(ledgers.iter().any(|ledger| {
            ledger.phase == super::super::super::journal::JournalZipCreatePhase::StageDiscarded
        }));
    }

    #[test]
    fn creates_one_archive_for_ordered_folder_and_unicode_file_roots() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let first = root.path().join("first folder");
        let second = root.path().join("second");
        let unicode_file = root.path().join("日本語.txt");
        std::fs::create_dir_all(first.join("nested")).unwrap();
        std::fs::create_dir(first.join("empty")).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("nested/a.txt"), b"alpha").unwrap();
        std::fs::write(second.join("b.txt"), b"beta").unwrap();
        std::fs::write(&unicode_file, b"unicode payload").unwrap();
        let output = root.path().join("combined.zip");
        let sources = vec![first.clone(), second.clone(), unicode_file.clone()];
        let guards: Vec<crate::operations::model::ZipCreateSourceGuard> = sources
            .iter()
            .map(|source| {
                let kind = if source.is_dir() {
                    crate::directory::EntryKind::Folder
                } else {
                    crate::directory::EntryKind::File
                };
                let captured =
                    crate::operations::model::test_zip_create_guard_for_kind(source, &output, kind);
                crate::operations::model::ZipCreateSourceGuard {
                    source_identity: captured.source_identity,
                    source_kind: captured.source_kind,
                    entry_name: captured.entry_name,
                }
            })
            .collect();
        let expected_source_identities = guards
            .iter()
            .map(|guard| format!("{:?}", guard.source_identity))
            .collect::<Vec<_>>();
        let parent_identity =
            crate::operations::model::test_zip_create_guard(&unicode_file, &output)
                .destination_parent_identity;
        let request = OperationRequest::create_zip_many(
            sources.clone(),
            output.clone(),
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: parent_identity,
                sources: guards,
            },
        )
        .unwrap();
        let (report, progress, ledgers) = run_zip_collect_progress(request);
        assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
        assert_exact_progress_totals(&report, &progress, 24);
        assert_eq!(report.final_counters.total_items, Some(1));
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(report.receipts[0].outcome, ItemOutcome::Completed);
        let ledger = ledgers
            .iter()
            .find(|ledger| {
                ledger.phase == super::super::super::journal::JournalZipCreatePhase::Published
            })
            .unwrap();
        assert_eq!(ledger.sources.len(), 3);
        assert_eq!(
            ledger
                .sources
                .iter()
                .map(|source| source.source_identity.clone())
                .collect::<Vec<_>>(),
            expected_source_identities
        );
        assert_eq!(
            ledger
                .sources
                .iter()
                .map(|source| String::from_utf16(&source.source.units).unwrap())
                .collect::<Vec<_>>(),
            sources
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        );
        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut expected = [
            "first folder/",
            "first folder/nested/",
            "first folder/empty/",
            "first folder/nested/a.txt",
            "second/",
            "second/b.txt",
            "日本語.txt",
        ]
        .to_vec();
        let mut actual = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected);
        for (name, bytes) in [
            ("first folder/nested/a.txt", b"alpha".as_slice()),
            ("second/b.txt", b"beta".as_slice()),
            ("日本語.txt", b"unicode payload".as_slice()),
        ] {
            let mut entry = archive.by_name(name).unwrap();
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).unwrap();
            assert_eq!(contents, bytes);
        }
        assert_eq!(std::fs::read(unicode_file).unwrap(), b"unicode payload");
        assert_eq!(std::fs::read(first.join("nested/a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(second.join("b.txt")).unwrap(), b"beta");
        drop(archive);
        let extracted = root.path().join("combined");
        let extract = OperationRequest::extract_zip(
            output.clone(),
            extracted.clone(),
            crate::operations::model::test_zip_create_guard(&output, &extracted),
        )
        .unwrap();
        let extract_report = run_builtin_extract(extract);
        assert_eq!(
            extract_report.outcome,
            TerminalOutcome::Succeeded,
            "{extract_report:?}"
        );
        assert_eq!(
            std::fs::read(extracted.join("first folder/nested/a.txt")).unwrap(),
            b"alpha"
        );
        assert!(extracted.join("first folder/empty").is_dir());
        assert_eq!(
            std::fs::read(extracted.join("second/b.txt")).unwrap(),
            b"beta"
        );
        assert_eq!(
            std::fs::read(extracted.join("日本語.txt")).unwrap(),
            b"unicode payload"
        );
    }

    #[test]
    fn zip_folder_preflight_accepts_exact_utf8_emitted_name_budget() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("é.txt"), b"utf8 name").unwrap();
        let output = root.path().join("source.zip");
        let guard = crate::operations::model::test_zip_create_guard_for_kind(
            &source,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let root_identity = identity_text(&guard.source_identity).unwrap();
        let parent_identity = identity_text(&guard.destination_parent_identity).unwrap();
        let prefix_bytes = "source/".len();
        let emitted_bytes = prefix_bytes + "é.txt".len();
        let limits = |name_bytes| super::super::windows::SourceTreeLimits {
            entries: 1,
            depth: 127,
            relative_name_bytes: name_bytes,
            record_bytes: usize::MAX,
            emitted_name_prefix_bytes: prefix_bytes,
            zip_utf8_name_bytes: true,
        };
        let exact = super::super::windows::preflight_pinned_source_tree_for_zip_with_limits(
            &source,
            &AtomicBool::new(false),
            &parent_identity,
            &root_identity,
            limits(emitted_bytes),
        )
        .unwrap();
        assert_eq!(exact.entries.len(), 1);
        drop(exact);
        let over = super::super::windows::preflight_pinned_source_tree_for_zip_with_limits(
            &source,
            &AtomicBool::new(false),
            &parent_identity,
            &root_identity,
            limits(emitted_bytes - 1),
        );
        assert!(matches!(
            over,
            Err(crate::operations::executor::CopyError::Unsupported)
        ));
    }

    #[test]
    fn cancellation_before_pinning_later_root_discards_earlier_pins_before_staging() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let later = root.path().join("later folder");
        std::fs::write(&first, b"first").unwrap();
        std::fs::create_dir(&later).unwrap();
        let output = root.path().join("cancelled.zip");
        let first_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &first,
            &output,
            crate::directory::EntryKind::File,
        );
        let later_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &later,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let parent_identity = first_guard.destination_parent_identity.clone();
        let request = OperationRequest::create_zip_many(
            vec![first.clone(), later.clone()],
            output.clone(),
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: parent_identity,
                sources: vec![
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: later_guard.source_identity,
                        source_kind: later_guard.source_kind,
                        entry_name: later_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        BEFORE_FOLDER_PREFLIGHT
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                later,
                Box::new(|cancelled| cancelled.store(true, Ordering::Release)),
            ));
        let (report, ledgers) = run_zip(request, false, false, false);
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert!(
            ledgers.is_empty(),
            "no StagePlanned may be emitted before all input pins succeed"
        );
        assert!(!output.exists());
        let released = root.path().join("released.txt");
        std::fs::rename(&first, released).expect("earlier source pin is released on cancellation");
    }

    #[test]
    fn replaced_later_root_kind_is_rejected_before_staging_and_releases_earlier_file_pin() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let later = root.path().join("later folder");
        let displaced = root.path().join("displaced folder");
        std::fs::write(&first, b"first").unwrap();
        std::fs::create_dir(&later).unwrap();
        let output = root.path().join("stale.zip");
        let first_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &first,
            &output,
            crate::directory::EntryKind::File,
        );
        let later_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &later,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let request = OperationRequest::create_zip_many(
            vec![first.clone(), later.clone()],
            output.clone(),
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity.clone(),
                sources: vec![
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: later_guard.source_identity,
                        source_kind: later_guard.source_kind,
                        entry_name: later_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        BEFORE_FOLDER_PREFLIGHT
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                later.clone(),
                Box::new(move |_| {
                    std::fs::rename(&later, &displaced).unwrap();
                    std::fs::write(&later, b"now a file").unwrap();
                }),
            ));
        let (report, ledgers) = run_zip(request, false, false, false);
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(
            ledgers.is_empty(),
            "stale non-first input must fail before StagePlanned"
        );
        assert!(!output.exists());
        let released = root.path().join("released.txt");
        std::fs::rename(&first, released)
            .expect("earlier source pin is released after later-root rejection");
    }

    #[test]
    fn folder_zip_worker_allows_source_path_classified_as_removable() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("removable-source");
        std::fs::create_dir_all(source.join("nested/empty")).unwrap();
        std::fs::write(source.join("nested/data.txt"), b"removable folder bytes").unwrap();
        let output = parent.path().join("removable-source.zip");
        let guard = crate::operations::model::test_zip_create_guard_for_kind(
            &source,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let source_identity = identity_text(&guard.source_identity).unwrap();
        let parent_identity = identity_text(&guard.destination_parent_identity).unwrap();
        let (actual_drive_type, actual_filesystem) =
            super::super::windows::archive_volume_info_for_test(&source).unwrap();
        assert!(
            actual_filesystem.eq_ignore_ascii_case("NTFS")
                || actual_filesystem.eq_ignore_ascii_case("exFAT"),
            "test temp volume must be NTFS or exFAT, got {actual_filesystem}"
        );

        // この fixture は drive type だけを差し替え、filesystem 名は実際の volume query で確認する。
        super::super::windows::set_archive_drive_type_override_for_test(&source, 2);
        let generic_result = super::super::windows::preflight_pinned_source_tree_with_shared_parent(
            &source,
            &AtomicBool::new(false),
            &parent_identity,
            &source_identity,
        );
        if actual_drive_type == 3 && actual_filesystem.eq_ignore_ascii_case("NTFS") {
            assert!(
                generic_result.is_ok(),
                "generic directory preflight retains fixed NTFS"
            );
        } else {
            assert!(
                generic_result.is_err(),
                "generic directory preflight retains fixed NTFS"
            );
        }
        drop(generic_result);
        let (report, ledgers) = run_zip(
            OperationRequest::create_zip(source.clone(), output.clone(), guard)
                .expect("captured ZIP request"),
            false,
            false,
            false,
        );
        assert!(
            !super::super::windows::archive_drive_type_override_pending_for_test(&source),
            "ZIP production preflight must consume the exact-path one-shot override"
        );
        assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
        assert!(source.is_dir());
        assert_eq!(
            std::fs::read(source.join("nested/data.txt")).unwrap(),
            b"removable folder bytes"
        );
        assert!(source.join("nested/empty").is_dir());

        let published = report.receipts[0]
            .created_archive_path
            .as_deref()
            .expect("published archive path");
        assert_eq!(published, output);
        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        for expected in [
            "removable-source/",
            "removable-source/nested/",
            "removable-source/nested/empty/",
            "removable-source/nested/data.txt",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}: {names:?}"
            );
        }
        let mut decoded = Vec::new();
        archive
            .by_name("removable-source/nested/data.txt")
            .unwrap()
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, b"removable folder bytes");
        assert_ne!(source_identity, parent_identity);
        let stage_identity_fact = ledgers
            .iter()
            .find(|ledger| {
                ledger.phase == super::super::super::journal::JournalZipCreatePhase::StageOwned
            })
            .and_then(|ledger| ledger.stage_identity.as_deref())
            .expect("stage identity");
        let published_identity = ledgers
            .iter()
            .find(|ledger| {
                ledger.phase == super::super::super::journal::JournalZipCreatePhase::Published
            })
            .and_then(|ledger| ledger.output_identity.as_deref())
            .expect("published identity");
        assert!(
            !stage_identity_fact.is_empty(),
            "owned stage identity is recorded"
        );
        assert_ne!(published_identity, source_identity);
        let output_identity = stage_identity(&File::open(published).unwrap()).unwrap();
        assert_eq!(output_identity, published_identity);
        assert_ne!(
            output_identity, source_identity,
            "archive has its own filesystem identity"
        );
        assert_ne!(
            output_identity, parent_identity,
            "archive is distinct from its parent directory"
        );
    }

    #[test]
    #[ignore = "streams a real >4 GiB source; run explicitly"]
    fn streams_actual_file_above_four_gibibytes_for_single_and_folder_routes() {
        let parent = tempfile::tempdir().unwrap();
        let folder = parent.path().join("large-folder");
        std::fs::create_dir_all(folder.join("empty")).unwrap();
        let source = folder.join("large.bin");
        let size = (1_u64 << 32) + 1;
        let marker = 0xA7;
        {
            let file = File::create(&source).unwrap();
            file.set_len(size).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&source)
                .unwrap();
            file.seek(SeekFrom::Start(size - 1)).unwrap();
            file.write_all(&[marker]).unwrap();
        }

        for (archive_path, request) in [
            (
                folder.join("single.zip"),
                OperationRequest::create_zip(
                    source.clone(),
                    folder.join("single.zip"),
                    crate::operations::model::test_zip_create_guard(
                        &source,
                        &folder.join("single.zip"),
                    ),
                )
                .unwrap(),
            ),
            (
                parent.path().join("folder.zip"),
                OperationRequest::create_zip(
                    folder.clone(),
                    parent.path().join("folder.zip"),
                    crate::operations::model::test_zip_create_guard_for_kind(
                        &folder,
                        &parent.path().join("folder.zip"),
                        crate::directory::EntryKind::Folder,
                    ),
                )
                .unwrap(),
            ),
        ] {
            let report = run_zip(request, false, false, false).0;
            assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
            let inspected = crate::zip_archive::inspect(
                &mut File::open(&archive_path).unwrap(),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(inspected.nodes.iter().any(|node| {
                node.kind == crate::zip_archive::EntryKind::File
                    && node
                        .components
                        .last()
                        .is_some_and(|name| name == "large.bin")
                    && node.declared_uncompressed_size == Some(size)
            }));
            assert_eq!(std::fs::metadata(&source).unwrap().len(), size);
            let mut source_tail = File::open(&source).unwrap();
            source_tail.seek(SeekFrom::Start(size - 1)).unwrap();
            let mut tail = [0];
            source_tail.read_exact(&mut tail).unwrap();
            assert_eq!(tail, [marker], "source tail must remain intact");
            let mut archive = zip::ZipArchive::new(File::open(&archive_path).unwrap()).unwrap();
            let target_name = if archive_path.ends_with("single.zip") {
                "large.bin"
            } else {
                "large-folder/large.bin"
            };
            let mut entry = archive.by_name(target_name).unwrap();
            assert_eq!(entry.size(), size);
            let mut buffer = vec![0_u8; CHUNK_BYTES];
            let mut total = 0_u64;
            let mut last = 0;
            loop {
                let read = entry.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                total += read as u64;
                last = buffer[read - 1];
            }
            assert_eq!(total, size);
            assert_eq!(last, marker);
            drop(entry);
            drop(archive);
            assert_file_local_headers_reserve_zip64(&archive_path);
            if archive_path.ends_with("folder.zip") {
                let mut archive = zip::ZipArchive::new(File::open(&archive_path).unwrap()).unwrap();
                assert!(archive.by_name("large-folder/empty/").unwrap().is_dir());
            } else {
                // 次の folder 作成にテスト自身の出力が混ざらないよう、検証済みの archive だけを消す。
                std::fs::remove_file(&archive_path).unwrap();
            }
        }
    }

    #[test]
    fn folder_zip_allows_write_shared_route_ancestor_without_weakening_source_pins() {
        let temp = tempfile::tempdir().unwrap();
        let shared_ancestor = temp.path().join("shared-ancestor");
        let source_parent = shared_ancestor.join("parent");
        let source = source_parent.join("source");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        let child = source.join("nested/data.txt");
        std::fs::write(&child, b"pinned source").unwrap();

        let writer =
            super::super::windows::open_add_children_ancestor_for_test(&shared_ancestor).unwrap();
        let read_only_open = super::super::windows::open_pinned_directory_with_share(
            &shared_ancestor,
            false,
            windows::Win32::Storage::FileSystem::FILE_SHARE_READ,
        );
        let read_only_error = match read_only_open {
            Err(crate::operations::executor::CopyError::Io(detail)) => detail,
            Err(other) => panic!("unexpected READ-only ancestor error: {other:?}"),
            Ok(_) => panic!("READ-only ancestor open unexpectedly succeeded"),
        };
        assert!(
            read_only_error.contains("80070020"),
            "READ-only ancestor error was not a sharing violation: {read_only_error}"
        );

        let parent_handle =
            super::super::windows::open_pinned_directory(&source_parent, false).unwrap();
        let parent_identity =
            super::super::windows::directory_identity_text(&parent_handle).unwrap();
        let root_handle = super::super::windows::open_pinned_directory(&source, false).unwrap();
        let root_identity = super::super::windows::directory_identity_text(&root_handle).unwrap();
        drop(root_handle);
        drop(parent_handle);

        let pinned =
            super::super::windows::preflight_pinned_source_tree_for_zip_with_shared_parent(
                &source,
                &AtomicBool::new(false),
                &parent_identity,
                &root_identity,
            )
            .unwrap();
        assert!(std::fs::write(&child, b"blocked").is_err());
        assert!(std::fs::rename(&source, source_parent.join("renamed-source")).is_err());
        assert!(std::fs::rename(&child, source.join("nested/renamed.txt")).is_err());
        let renamed_ancestor = temp.path().join("renamed-ancestor");
        assert!(std::fs::rename(&shared_ancestor, &renamed_ancestor).is_err());
        drop(writer);
        assert!(std::fs::rename(&shared_ancestor, &renamed_ancestor).is_err());
        drop(pinned);
        std::fs::rename(&shared_ancestor, &renamed_ancestor).unwrap();
        std::fs::rename(&renamed_ancestor, &shared_ancestor).unwrap();

        let writer =
            super::super::windows::open_add_children_ancestor_for_test(&shared_ancestor).unwrap();
        let output = source_parent.join("source.zip");
        let guard = crate::operations::model::test_zip_create_guard_for_kind(
            &source,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let request = OperationRequest::create_zip(source, output.clone(), guard).unwrap();
        let (report, _) = run_zip(request, false, false, false);
        assert!(
            report
                .receipts
                .iter()
                .all(|receipt| receipt.outcome == ItemOutcome::Completed)
        );
        assert!(output.is_file());
        assert_file_local_headers_reserve_zip64(&output);
        drop(writer);
    }

    #[test]
    fn replaced_folder_root_is_rejected_without_publication() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("root");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("item.txt"), b"captured").unwrap();
        let output = parent.path().join("root.zip");
        let request = OperationRequest::create_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard_for_kind(
                &source,
                &output,
                crate::directory::EntryKind::Folder,
            ),
        )
        .unwrap();
        let displaced = parent.path().join("displaced");
        std::fs::rename(&source, &displaced).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("item.txt"), b"replacement").unwrap();
        let report = run_zip(request, false, false, false).0;
        assert_eq!(report.outcome, TerminalOutcome::Failed, "{report:?}");
        assert!(!output.exists());

        let source = parent.path().join("deleted-and-recreated-root");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("item.txt"), b"captured before delete").unwrap();
        let output = parent.path().join("deleted-and-recreated-root.zip");
        let request = OperationRequest::create_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard_for_kind(
                &source,
                &output,
                crate::directory::EntryKind::Folder,
            ),
        )
        .unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("item.txt"), b"replacement").unwrap();
        let report = run_zip(request, false, false, false).0;
        assert_eq!(report.outcome, TerminalOutcome::Failed, "{report:?}");
        assert!(!output.exists());
    }

    #[test]
    fn reparse_descendant_is_rejected_when_symlink_creation_is_supported() {
        use std::os::windows::fs::symlink_file;

        let parent = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let linked_root = parent.path().join("linked-root");
        std::fs::create_dir(&linked_root).unwrap();
        let target = parent.path().join("outside.txt");
        std::fs::write(&target, b"outside").unwrap();
        if let Err(error) = symlink_file(&target, linked_root.join("link.txt")) {
            eprintln!("skipping reparse-descendant assertion: symlink creation failed: {error}");
            return;
        }
        let linked_output = parent.path().join("linked-root.zip");
        let linked_request = OperationRequest::create_zip(
            linked_root.clone(),
            linked_output.clone(),
            crate::operations::model::test_zip_create_guard_for_kind(
                &linked_root,
                &linked_output,
                crate::directory::EntryKind::Folder,
            ),
        )
        .unwrap();
        let report = run_zip(linked_request, false, false, false).0;
        assert_eq!(report.outcome, TerminalOutcome::Failed, "{report:?}");
        assert!(!linked_output.exists());
    }

    #[test]
    fn root_replacement_after_path_check_is_rejected_before_descendant_open() {
        use std::os::windows::fs::OpenOptionsExt;

        let parent = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = parent.path().join("root");
        let displaced = parent.path().join("displaced");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("original.txt"), b"captured").unwrap();
        let output = parent.path().join("root.zip");
        let request = OperationRequest::create_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard_for_kind(
                &source,
                &output,
                crate::directory::EntryKind::Folder,
            ),
        )
        .unwrap();
        let child_pin = Arc::new(std::sync::Mutex::new(None));
        let hook_child_pin = child_pin.clone();
        BEFORE_FOLDER_PREFLIGHT
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                source.clone(),
                Box::new(move |_| {
                    std::fs::rename(&source, &displaced).unwrap();
                    std::fs::create_dir(&source).unwrap();
                    let child = source.join("locked.txt");
                    std::fs::write(&child, b"must not be opened").unwrap();
                    let handle = std::fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(child)
                        .unwrap();
                    *hook_child_pin.lock().unwrap() = Some(handle);
                }),
            ));

        let report = run_zip(request, false, false, false).0;
        assert!(child_pin.lock().unwrap().is_some());
        assert_eq!(report.outcome, TerminalOutcome::Failed, "{report:?}");
        assert!(
            report.receipts[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("identity changed")
        );
        assert!(!output.exists());
    }

    #[test]
    fn folder_entry_limit_fails_before_creating_an_archive() {
        let parent = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = parent.path().join("large");
        std::fs::create_dir(&source).unwrap();
        for index in 0..1025 {
            std::fs::write(source.join(format!("{index:04}.txt")), []).unwrap();
        }
        let output = parent.path().join("large.zip");
        let request = OperationRequest::create_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard_for_kind(
                &source,
                &output,
                crate::directory::EntryKind::Folder,
            ),
        )
        .unwrap();
        let report = run_zip(request, false, false, false).0;
        assert_eq!(report.outcome, TerminalOutcome::Failed, "{report:?}");
        assert!(!output.exists());
    }

    #[cfg(windows)]
    #[test]
    fn two_file_roots_report_the_complete_byte_total_before_streaming() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let first = root.path().join("first.bin");
        let second = root.path().join("second.bin");
        let first_bytes = vec![0x1a; 1024 * 1024];
        let second_bytes = vec![0x2b; 9 * 1024 * 1024];
        std::fs::write(&first, &first_bytes).unwrap();
        std::fs::write(&second, &second_bytes).unwrap();
        let output = root.path().join("first.zip");
        let first_guard = crate::operations::model::test_zip_create_guard(&first, &output);
        let second_guard = crate::operations::model::test_zip_create_guard(&second, &output);
        let request = OperationRequest::create_zip_many(
            vec![first.clone(), second.clone()],
            output,
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity.clone(),
                sources: vec![first_guard, second_guard]
                    .into_iter()
                    .map(|guard| crate::operations::model::ZipCreateSourceGuard {
                        source_identity: guard.source_identity,
                        source_kind: guard.source_kind,
                        entry_name: guard.entry_name,
                    })
                    .collect(),
            },
        )
        .unwrap();

        let (report, progress, _) = run_zip_collect_progress(request);
        assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
        assert_exact_progress_totals(
            &report,
            &progress,
            (first_bytes.len() + second_bytes.len()) as u64,
        );
    }

    #[test]
    fn aggregate_folder_entry_limit_rejects_individually_valid_roots_before_staging() {
        let parent = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let first = parent.path().join("first");
        let second = parent.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for index in 0..511 {
            std::fs::write(first.join(format!("a{index:03}.txt")), []).unwrap();
            std::fs::write(second.join(format!("b{index:03}.txt")), []).unwrap();
        }
        let output = parent.path().join("combined-exact.zip");
        let first_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &first,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let second_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &second,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let exact_request = OperationRequest::create_zip_many(
            vec![first.clone(), second.clone()],
            output.clone(),
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity,
                sources: vec![
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: second_guard.source_identity,
                        source_kind: second_guard.source_kind,
                        entry_name: second_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        let (report, _) = run_zip(exact_request, false, false, false);
        assert_eq!(
            report.outcome,
            TerminalOutcome::Succeeded,
            "exact aggregate entry limit must be accepted: {report:?}"
        );
        let over_output = parent.path().join("combined-over.zip");
        std::fs::write(second.join("b511.txt"), []).unwrap();
        let first_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &first,
            &over_output,
            crate::directory::EntryKind::Folder,
        );
        let second_guard = crate::operations::model::test_zip_create_guard_for_kind(
            &second,
            &over_output,
            crate::directory::EntryKind::Folder,
        );
        let request = OperationRequest::create_zip_many(
            vec![first, second],
            over_output.clone(),
            crate::operations::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity,
                sources: vec![
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    crate::operations::model::ZipCreateSourceGuard {
                        source_identity: second_guard.source_identity,
                        source_kind: second_guard.source_kind,
                        entry_name: second_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        let (report, ledgers) = run_zip(request, false, false, false);
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(
            ledgers.is_empty(),
            "the aggregate 1024-entry budget is enforced before StagePlanned"
        );
        assert!(!over_output.exists());
    }

    #[test]
    fn emitted_folder_depth_accepts_limit_and_rejects_one_over_before_staging() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("root");
        std::fs::create_dir(&source).unwrap();
        let mut deepest = source.clone();
        for _ in 0..2 {
            deepest.push("d");
            std::fs::create_dir(&deepest).unwrap();
        }
        let output = parent.path().join("depth-exact.zip");
        ZIP_CREATE_LIMITS_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                output.clone(),
                ZipCreateLimits {
                    entries: MAX_FOLDER_ARCHIVE_ENTRIES,
                    depth: 3,
                    name_bytes: MAX_FOLDER_ARCHIVE_NAME_BYTES,
                },
            ));
        let guard = crate::operations::model::test_zip_create_guard_for_kind(
            &source,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let (report, _) = run_zip(
            OperationRequest::create_zip(source.clone(), output.clone(), guard).unwrap(),
            false,
            false,
            false,
        );
        assert_eq!(
            report.outcome,
            TerminalOutcome::Succeeded,
            "the exact test depth should fit: {report:?}"
        );

        deepest.push("d");
        std::fs::create_dir(&deepest).unwrap();
        let over_output = parent.path().join("depth-over.zip");
        ZIP_CREATE_LIMITS_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                over_output.clone(),
                ZipCreateLimits {
                    entries: MAX_FOLDER_ARCHIVE_ENTRIES,
                    depth: 3,
                    name_bytes: MAX_FOLDER_ARCHIVE_NAME_BYTES,
                },
            ));
        let guard = crate::operations::model::test_zip_create_guard_for_kind(
            &source,
            &over_output,
            crate::directory::EntryKind::Folder,
        );
        let (report, ledgers) = run_zip(
            OperationRequest::create_zip(source.clone(), over_output.clone(), guard).unwrap(),
            false,
            false,
            false,
        );
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(
            ledgers.is_empty(),
            "one over the test depth must reject before StagePlanned"
        );
        assert!(!over_output.exists());
        let released = parent.path().join("released-root");
        std::fs::rename(&source, released).expect("folder pins release after depth rejection");
    }

    #[test]
    fn aggregate_utf8_name_budget_accepts_exact_and_rejects_over_before_staging() {
        let parent = tempfile::tempdir().unwrap();
        let first = parent.path().join("first.txt");
        let folder = parent.path().join("folder");
        std::fs::write(&first, b"first").unwrap();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("childname.txt"), b"child").unwrap();
        let sources = vec![first.clone(), folder.clone()];
        let make_request = |output: &Path| {
            let first_guard = crate::operations::model::test_zip_create_guard_for_kind(
                &first,
                output,
                crate::directory::EntryKind::File,
            );
            let folder_guard = crate::operations::model::test_zip_create_guard_for_kind(
                &folder,
                output,
                crate::directory::EntryKind::Folder,
            );
            OperationRequest::create_zip_many(
                sources.clone(),
                output.to_path_buf(),
                crate::operations::model::ZipArchiveCreateGuard {
                    destination_parent_identity: first_guard.destination_parent_identity,
                    sources: vec![
                        crate::operations::model::ZipCreateSourceGuard {
                            source_identity: first_guard.source_identity,
                            source_kind: first_guard.source_kind,
                            entry_name: first_guard.entry_name,
                        },
                        crate::operations::model::ZipCreateSourceGuard {
                            source_identity: folder_guard.source_identity,
                            source_kind: folder_guard.source_kind,
                            entry_name: folder_guard.entry_name,
                        },
                    ],
                },
            )
            .unwrap()
        };

        let exact_output = parent.path().join("names-exact.zip");
        ZIP_CREATE_LIMITS_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                exact_output.clone(),
                ZipCreateLimits {
                    entries: MAX_FOLDER_ARCHIVE_ENTRIES,
                    depth: MAX_FOLDER_ARCHIVE_DEPTH,
                    name_bytes: 36,
                },
            ));
        let (report, _) = run_zip(make_request(&exact_output), false, false, false);
        assert_eq!(
            report.outcome,
            TerminalOutcome::Succeeded,
            "36 UTF8 bytes are the exact combined count"
        );

        let over_output = parent.path().join("names-over.zip");
        ZIP_CREATE_LIMITS_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push((
                over_output.clone(),
                ZipCreateLimits {
                    entries: MAX_FOLDER_ARCHIVE_ENTRIES,
                    depth: MAX_FOLDER_ARCHIVE_DEPTH,
                    name_bytes: 35,
                },
            ));
        let (report, ledgers) = run_zip(make_request(&over_output), false, false, false);
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(
            ledgers.is_empty(),
            "aggregate name overflow must reject before StagePlanned"
        );
        assert!(!over_output.exists());
        let released = parent.path().join("released-first.txt");
        std::fs::rename(&first, released)
            .expect("first root pin is released after aggregate-name rejection");
    }

    #[test]
    fn cancellation_during_stream_cleans_only_the_owned_stage() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("big.bin");
        std::fs::write(&source, vec![0x5a; CHUNK_BYTES * 3]).unwrap();
        let output = root.path().join("big.zip");
        let report = run_zip(
            OperationRequest::create_zip(
                source.clone(),
                output.clone(),
                crate::operations::model::test_zip_create_guard(&source, &output),
            )
            .unwrap(),
            true,
            false,
            false,
        )
        .0;
        assert_eq!(report.outcome, TerminalOutcome::Cancelled, "{report:?}");
        assert!(!output.exists());
    }

    #[test]
    fn external_output_reuse_after_publish_intent_keeps_sentinel_untouched() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        std::fs::write(&source, b"source").unwrap();
        let output = root.path().join("target.zip");
        let report = run_zip(
            OperationRequest::create_zip(
                source.clone(),
                output.clone(),
                crate::operations::model::test_zip_create_guard(&source, &output),
            )
            .unwrap(),
            false,
            true,
            false,
        )
        .0;
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert_eq!(std::fs::read(output).unwrap(), b"sentinel");
    }

    #[test]
    fn post_publish_identity_failure_keeps_output_and_publish_intent() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        std::fs::write(&source, b"source bytes").unwrap();
        let output = root.path().join("target.zip");
        let request = OperationRequest::create_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard(&source, &output),
        )
        .unwrap();
        *OUTPUT_IDENTITY_FAILURE_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some(output.clone());

        let (report, ledgers) = run_zip(request, false, false, false);
        assert_eq!(report.outcome, TerminalOutcome::Unknown, "{report:?}");
        assert_eq!(
            report.receipts[0].created_archive_path.as_deref(),
            Some(output.as_path())
        );
        assert!(output.exists());
        assert!(ledgers.iter().any(|ledger| {
            ledger.phase == super::super::super::journal::JournalZipCreatePhase::PublishIntent
        }));
        assert!(!ledgers.iter().any(|ledger| {
            ledger.phase == super::super::super::journal::JournalZipCreatePhase::StageDiscarded
                || ledger.output_identity.is_some()
        }));
    }

    #[test]
    fn replaced_source_is_rejected_before_publishing() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        std::fs::write(&source, b"captured source").unwrap();
        let output = root.path().join("target.zip");
        let request = OperationRequest::create_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard(&source, &output),
        )
        .unwrap();
        let displaced = root.path().join("displaced.txt");
        std::fs::rename(&source, &displaced).unwrap();
        std::fs::write(&source, b"replacement source").unwrap();
        let report = run_zip(request, false, false, false).0;
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(!output.exists());
    }

    #[test]
    fn cancellation_while_published_receipt_ack_is_pending_reports_unknown_with_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        std::fs::write(&source, b"source").unwrap();
        let output = root.path().join("target.zip");
        let report = run_zip(
            OperationRequest::create_zip(
                source.clone(),
                output.clone(),
                crate::operations::model::test_zip_create_guard(&source, &output),
            )
            .unwrap(),
            false,
            false,
            true,
        )
        .0;
        assert_eq!(report.outcome, TerminalOutcome::Unknown, "{report:?}");
        assert_eq!(
            report.receipts[0].created_archive_path.as_deref(),
            Some(output.as_path())
        );
        assert!(output.exists());
    }

    #[test]
    fn owned_stage_rejects_external_writer_and_keeps_its_bytes() {
        use std::io::Write as _;

        let root = tempfile::tempdir().unwrap();
        let stage_path = root.path().join("owned.stage");
        let mut stage = create_stage_file(&stage_path).unwrap();
        stage.write_all(b"owned zip bytes").unwrap();
        stage.flush().unwrap();

        let external_writer = std::fs::OpenOptions::new().write(true).open(&stage_path);
        assert!(
            external_writer.is_err(),
            "external writer must be denied while stage handle is held"
        );
        assert_eq!(std::fs::read(&stage_path).unwrap(), b"owned zip bytes");
    }

    #[cfg(windows)]
    #[test]
    fn destination_route_lease_blocks_parent_rename_through_the_publish_barrier() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("A");
        let parent = ancestor.join("B");
        std::fs::create_dir_all(&parent).unwrap();
        let source = parent.join("source.txt");
        let output = parent.join("source.zip");
        std::fs::write(&source, b"source").unwrap();
        let guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let displaced_ancestor = ancestor.with_extension("displaced");
        let mut attempted_during_acquisition = false;
        let route = super::super::windows::lock_virtual_destination_with_hook_for_test(
            &parent,
            &guard.destination_parent_identity,
            |locked| {
                if locked == ancestor && !attempted_during_acquisition {
                    attempted_during_acquisition = true;
                    assert!(
                        std::fs::rename(&ancestor, &displaced_ancestor).is_err(),
                        "an ancestor must be pinned before the child is opened"
                    );
                }
            },
        )
        .unwrap();
        assert!(attempted_during_acquisition);
        let displaced = root.path().with_extension("displaced");
        assert!(
            std::fs::rename(root.path(), &displaced).is_err(),
            "an ancestor cannot be redirected while the worker awaits a journal ACK"
        );
        assert!(source.exists());
        drop(route);
        assert!(std::fs::rename(root.path(), &displaced).is_ok());
        assert!(std::fs::rename(&displaced, root.path()).is_ok());
    }
}
