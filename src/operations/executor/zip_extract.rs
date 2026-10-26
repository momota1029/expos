use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    sync::mpsc::Receiver,
};

#[cfg(test)]
use std::io::SeekFrom;
#[cfg(test)]
use std::{cell::Cell, cmp::Ordering};

#[cfg(test)]
use crate::zip_archive::resolve_zip64_extra;
#[cfg(test)]
use crate::zip_archive::validated_reader;

use crate::operations::{
    executor::{ExecutorFact as Fact, WorkerControl},
    journal::{
        JournalDirectoryNodeKind as NodeKind, JournalPath, JournalZipExtract,
        JournalZipExtractPhase as Phase,
    },
    model::{
        AttemptToken, ItemId, ItemOutcome, ItemReceipt, OperationKind, OperationRequest,
        ProgressCounters, RetryPlan, TerminalOutcome, TerminalReport,
    },
};

const MAX_CENTRAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAX_DEPTH: usize = 128;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ZIP64_EXTENSION: u64 = 64 * 1024;
const CHUNK: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestNode {
    components: Vec<String>,
    kind: NodeKind,
    explicit: bool,
    entry_index: Option<usize>,
}

#[derive(Debug)]
struct ExtractError(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerError {
    Cancelled,
    AcknowledgementFailed,
}

impl From<LedgerError> for ExtractError {
    fn from(error: LedgerError) -> Self {
        match error {
            LedgerError::Cancelled => Self("cancelled".into()),
            LedgerError::AcknowledgementFailed => {
                Self("extraction journal acknowledgement failed".into())
            }
        }
    }
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<crate::zip_archive::ZipReadError> for ExtractError {
    fn from(error: crate::zip_archive::ZipReadError) -> Self {
        Self(error.0)
    }
}

pub(super) fn execute(
    token: AttemptToken,
    request: &OperationRequest,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) {
    let (guards, parent_identity) = match request.zip_create_guard.as_ref() {
        Some(crate::operations::model::ZipOperationGuard::Extract(guard)) => (
            vec![crate::operations::model::ZipCreateSourceGuard {
                source_identity: guard.source_identity.clone(),
                source_kind: guard.source_kind,
                entry_name: guard.entry_name.clone(),
            }],
            Some(guard.destination_parent_identity.clone()),
        ),
        Some(crate::operations::model::ZipOperationGuard::ExtractMany(guard)) => (
            guard.sources.clone(),
            Some(guard.destination_parent_identity.clone()),
        ),
        _ => (Vec::new(), None),
    };
    let count = request.sources.len();
    let mut counters = ProgressCounters {
        total_items: Some(count as u64),
        ..Default::default()
    };
    let mut receipts = Vec::with_capacity(count);
    let mut stop = false;
    for index in 0..count {
        let item = ItemId(index as u32);
        let source = &request.sources[index];
        let output = source
            .parent()
            .zip(source.file_stem())
            .map(|(parent, stem)| parent.join(stem));
        if stop
            || cancelled.load(AtomicOrdering::Acquire)
            || index >= guards.len()
            || output.is_none()
        {
            let was_cancelled = stop || cancelled.load(AtomicOrdering::Acquire);
            receipts.push(ItemReceipt {
                item,
                outcome: if was_cancelled {
                    ItemOutcome::CancelledBeforeStart
                } else {
                    ItemOutcome::Failed
                },
                detail: Some(
                    if was_cancelled {
                        "cancelled before extraction started"
                    } else {
                        "invalid captured ZIP extraction request"
                    }
                    .into(),
                ),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            stop |= was_cancelled;
            continue;
        }
        let mut item_request = request.clone();
        item_request.sources = vec![source.clone()];
        item_request.destination = output.clone();
        item_request.zip_create_guard = Some(crate::operations::model::ZipOperationGuard::Extract(
            crate::operations::model::ZipCreateGuard {
                source_identity: guards[index].source_identity.clone(),
                destination_parent_identity: parent_identity
                    .clone()
                    .expect("validated extraction guard"),
                source_kind: guards[index].source_kind,
                entry_name: guards[index].entry_name.clone(),
            },
        ));
        let mut retained_location = None;
        let result = extract(
            token,
            item,
            &item_request,
            cancelled,
            controls,
            emit,
            &mut counters,
            &mut retained_location,
        );
        let receipt = match result {
            Ok(path) => {
                counters.processed_items += 1;
                ItemReceipt {
                    item,
                    outcome: ItemOutcome::Completed,
                    detail: None,
                    created_folder_path: Some(path),
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }
            }
            Err(error) if error.0 == "cancelled" => {
                stop = true;
                ItemReceipt {
                    item,
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: Some(error.0),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }
            }
            Err(error) if error.0.starts_with("unknown:") => {
                stop = true;
                ItemReceipt {
                    item,
                    outcome: ItemOutcome::Unknown,
                    detail: Some("publication outcome requires inspection".into()),
                    created_folder_path: retained_location,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }
            }
            Err(error) if error.0.starts_with("partial:") => {
                stop = true;
                ItemReceipt {
                    item,
                    outcome: ItemOutcome::Partial,
                    detail: Some(error.0),
                    created_folder_path: retained_location,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }
            }
            Err(error) => ItemReceipt {
                item,
                outcome: ItemOutcome::Failed,
                detail: Some(format!("ZIP extraction failed: {error}")),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            },
        };
        if receipt.outcome == ItemOutcome::CancelledBeforeStart {
            stop = true;
        }
        receipts.push(receipt);
        if count > 1 {
            counters.total_bytes = None;
        }
    }
    let has_unknown = receipts
        .iter()
        .any(|receipt| receipt.outcome == ItemOutcome::Unknown);
    let has_partial = receipts
        .iter()
        .any(|receipt| receipt.outcome == ItemOutcome::Partial);
    let has_cancelled = receipts
        .iter()
        .any(|receipt| receipt.outcome == ItemOutcome::CancelledBeforeStart);
    let has_failed = receipts
        .iter()
        .any(|receipt| receipt.outcome == ItemOutcome::Failed);
    let completed = receipts
        .iter()
        .filter(|receipt| receipt.outcome == ItemOutcome::Completed)
        .count();
    if count > 1 {
        counters.total_bytes = None;
    }
    let outcome = if has_unknown {
        TerminalOutcome::Unknown
    } else if has_partial {
        TerminalOutcome::Partial
    } else if has_cancelled {
        TerminalOutcome::Cancelled
    } else if has_failed && completed > 0 {
        TerminalOutcome::Partial
    } else if has_failed {
        TerminalOutcome::Failed
    } else {
        TerminalOutcome::Succeeded
    };
    let retry = if matches!(
        outcome,
        TerminalOutcome::Succeeded | TerminalOutcome::Cancelled
    ) {
        RetryPlan::NotAllowed
    } else {
        RetryPlan::RequiresReinspection
    };
    let parent_hint = request
        .sources
        .first()
        .and_then(|source| source.parent())
        .map(|directory| crate::operations::model::FsChangeHint {
            directory: directory.to_path_buf(),
        })
        .into_iter()
        .collect();
    emit(Fact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: counters,
            receipts,
            retry,
            change_hints: parent_hint,
        },
    });
}

fn extract(
    token: AttemptToken,
    item: ItemId,
    request: &OperationRequest,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
    counters: &mut ProgressCounters,
    retained_location: &mut Option<PathBuf>,
) -> Result<PathBuf, ExtractError> {
    if request.kind != OperationKind::ExtractZip
        || request.sources.len() != 1
        || request.virtual_source.is_some()
    {
        return Err(ExtractError("invalid captured extraction request".into()));
    }
    let source = &request.sources[0];
    let destination = request
        .destination
        .as_deref()
        .ok_or_else(|| ExtractError("missing sibling folder".into()))?;
    let guard = request
        .zip_create_guard
        .as_ref()
        .ok_or_else(|| ExtractError("missing captured identity".into()))?;
    let crate::operations::model::ZipOperationGuard::Extract(guard) = guard else {
        return Err(ExtractError("invalid ZIP extraction identity".into()));
    };
    let parent = source
        .parent()
        .ok_or_else(|| ExtractError("source has no parent".into()))?;
    if destination.parent() != Some(parent)
        || !source
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.eq_ignore_ascii_case("zip"))
        || guard.source_kind != crate::directory::EntryKind::File
        || guard.entry_name != source.file_name().and_then(|v| v.to_str()).unwrap_or("")
    {
        return Err(ExtractError("invalid sibling ZIP target".into()));
    }
    if cancelled.load(AtomicOrdering::Acquire) {
        return Err(ExtractError("cancelled".into()));
    }
    let parent_route = crate::directory::EntrySourceIdentity::from_path_no_follow(
        parent,
        crate::directory::EntryKind::Folder,
    )
    .ok_or_else(|| ExtractError("parent identity changed".into()))?;
    if parent_route.filesystem_identity() != guard.destination_parent_identity {
        return Err(ExtractError("parent identity changed".into()));
    }
    #[cfg(windows)]
    let _parent_pins =
        super::windows::lock_virtual_destination(parent, Some(&guard.destination_parent_identity))
            .map_err(|e| ExtractError(format!("parent identity changed: {e:?}")))?;
    #[cfg(not(windows))]
    return Err(ExtractError(
        "ZIP extraction requires Windows handle-relative filesystem operations".into(),
    ));
    #[cfg(windows)]
    {
        let mut input = open_source(source)?;
        verify_source(source, &input, &guard.source_identity)?;
        ensure_output_absent(destination)?;
        crate::zip_archive::with_validated_archive(
            &mut input,
            cancelled,
            |archive, domain_manifest| {
                let manifest = domain_manifest
                    .iter()
                    .map(|node| ManifestNode {
                        components: node.components.clone(),
                        kind: match node.kind {
                            crate::zip_archive::EntryKind::File => NodeKind::File,
                            crate::zip_archive::EntryKind::Directory => NodeKind::Directory,
                        },
                        explicit: node.explicit,
                        entry_index: node.entry_index,
                    })
                    .collect::<Vec<_>>();
                let declared_total = validate_declared_total(domain_manifest)?;
                counters.total_bytes = Some(declared_total);
                if cancelled.load(AtomicOrdering::Acquire) {
                    return Err(ExtractError("cancelled".into()));
                }
                let stage_path = unique_stage_path(parent, token)?;
                let mut ledger = JournalZipExtract {
                    source: JournalPath::from_path(source),
                    output: JournalPath::from_path(destination),
                    stage: JournalPath::from_path(&stage_path),
                    source_identity: format!("{:?}", guard.source_identity),
                    parent_identity: format!("{:?}", guard.destination_parent_identity),
                    stage_identity: None,
                    phase: Phase::StagePlanned,
                };
                if let Err(error) = journal_ledger(token, item, &ledger, controls, emit) {
                    if error == LedgerError::Cancelled {
                        ledger.phase = Phase::StageDiscarded;
                        journal_ledger(token, item, &ledger, controls, emit)?;
                        return Err(ExtractError::from(LedgerError::Cancelled));
                    }
                    return Err(error.into());
                }
                if cancelled.load(AtomicOrdering::Acquire) {
                    ledger.phase = Phase::StageDiscarded;
                    journal_ledger(token, item, &ledger, controls, emit)?;
                    return Err(ExtractError("cancelled".into()));
                }
                let lease =
                    super::windows::create_attempt_staging_root(&stage_path).map_err(|e| {
                        ExtractError(format!(
                            "could not create exclusive extraction stage: {e:?}"
                        ))
                    })?;
                ledger.stage_identity = Some(lease.root_identity.clone());
                ledger.phase = Phase::StageOwned;
                if let Err(error) = journal_ledger(token, item, &ledger, controls, emit) {
                    let original = ExtractError::from(error);
                    let cleaned = clean_stage_failure(lease, Vec::new(), &stage_path, original);
                    if cleaned.0.starts_with("partial:") {
                        *retained_location = Some(stage_path.clone());
                    }
                    if error == LedgerError::Cancelled && cleaned.0 == "cancelled" {
                        ledger.phase = Phase::StageDiscarded;
                        journal_ledger(token, item, &ledger, controls, emit)?;
                        return Err(ExtractError::from(LedgerError::Cancelled));
                    }
                    return Err(cleaned);
                }
                let (staged, bytes) = match materialize(archive, &manifest, lease, cancelled) {
                    Ok(result) => result,
                    Err(error) => {
                        if error.0.starts_with("partial:") {
                            *retained_location = Some(stage_path.clone());
                        } else {
                            ledger.phase = Phase::StageDiscarded;
                            journal_ledger(token, item, &ledger, controls, emit)?;
                        }
                        return Err(error);
                    }
                };
                counters.processed_bytes = counters
                    .processed_bytes
                    .checked_add(bytes)
                    .unwrap_or(u64::MAX);
                if cancelled.load(AtomicOrdering::Acquire) {
                    if let Err(error) = cleanup_stage(staged, &stage_path) {
                        *retained_location = Some(stage_path.clone());
                        return Err(error);
                    }
                    ledger.phase = Phase::StageDiscarded;
                    journal_ledger(token, item, &ledger, controls, emit)?;
                    return Err(ExtractError("cancelled".into()));
                }
                ledger.phase = Phase::PublishIntent;
                if let Err(error) = journal_ledger(token, item, &ledger, controls, emit) {
                    if let Err(cleanup_error) = cleanup_stage(staged, &stage_path) {
                        *retained_location = Some(stage_path.clone());
                        return Err(cleanup_error);
                    }
                    ledger.phase = Phase::StageDiscarded;
                    journal_ledger(token, item, &ledger, controls, emit)?;
                    return Err(error.into());
                }
                let published = match super::windows::publish_and_verify_staged_tree(
                    staged,
                    destination,
                    cancelled,
                ) {
                    Ok(tree) => tree,
                    Err(super::windows::PublishTreeFailure::BeforePublish(
                        super::CopyError::DestinationExists,
                    )) => {
                        ledger.phase = Phase::StageDiscarded;
                        journal_ledger(token, item, &ledger, controls, emit)?;
                        return Err(ExtractError("output folder already exists".into()));
                    }
                    Err(super::windows::PublishTreeFailure::BeforePublish(error)) => {
                        if matches!(error, super::CopyError::PrePublishPartial(_)) {
                            // Keep the owned stage identity and PublishIntent for startup recovery.
                            *retained_location = Some(stage_path.clone());
                            return Err(before_publish_cleanup_unconfirmed(
                                error,
                                &stage_path,
                                ledger.stage_identity.as_deref(),
                            ));
                        }
                        ledger.phase = Phase::StageDiscarded;
                        journal_ledger(token, item, &ledger, controls, emit)?;
                        if matches!(error, super::CopyError::Cancelled) {
                            return Err(ExtractError("cancelled".into()));
                        }
                        return Err(ExtractError(format!(
                            "publication failed before rename: {error:?}"
                        )));
                    }
                    Err(super::windows::PublishTreeFailure::AfterPublish { error, tree }) => {
                        drop(tree);
                        *retained_location = Some(destination.to_path_buf());
                        return Err(ExtractError(format!(
                            "partial:{} (post-publication verification failed: {error:?})",
                            destination.display()
                        )));
                    }
                };
                drop(published);
                ledger.phase = Phase::Published;
                if journal_ledger(token, item, &ledger, controls, emit).is_err() {
                    *retained_location = Some(destination.to_path_buf());
                    return Err(ExtractError(format!("unknown:{}", destination.display())));
                }
                Ok(destination.to_path_buf())
            },
        )
    }
}

#[cfg(windows)]
fn ensure_output_absent(destination: &Path) -> Result<(), ExtractError> {
    match std::fs::symlink_metadata(destination) {
        Ok(_) => Err(ExtractError("output folder already exists".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExtractError(format!(
            "could not verify output path is absent: {error}"
        ))),
    }
}

fn before_publish_cleanup_unconfirmed(
    error: super::CopyError,
    stage_path: &Path,
    stage_identity: Option<&str>,
) -> ExtractError {
    ExtractError(format!(
        "partial:stage {} ({}) cleanup unconfirmed: {error:?}",
        stage_path.display(),
        stage_identity.unwrap_or("unknown identity")
    ))
}

#[cfg(test)]
type ScannedArchive = crate::zip_archive::ScannedArchive;
#[cfg(test)]
type CentralEntry = crate::zip_archive::CentralEntry;

#[cfg(test)]
fn scan_central_directory(
    file: &mut File,
    cancelled: &AtomicBool,
) -> Result<ScannedArchive, ExtractError> {
    crate::zip_archive::scan_central_directory(file, cancelled)
        .map_err(|error| ExtractError(error.0))
}

#[cfg(test)]
fn build_manifest_from_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    central: &[CentralEntry],
) -> Result<Vec<ManifestNode>, ExtractError> {
    crate::zip_archive::build_manifest_from_archive(archive, central)
        .map(|nodes| {
            nodes
                .into_iter()
                .map(|node| ManifestNode {
                    components: node.components,
                    kind: match node.kind {
                        crate::zip_archive::EntryKind::File => NodeKind::File,
                        crate::zip_archive::EntryKind::Directory => NodeKind::Directory,
                    },
                    explicit: node.explicit,
                    entry_index: node.entry_index,
                })
                .collect()
        })
        .map_err(|error| ExtractError(error.0))
}

#[cfg(test)]
fn compare_paths(a: &[String], b: &[String]) -> Ordering {
    crate::zip_archive::compare_paths(a, b)
}

fn validate_declared_total(
    manifest: &[crate::zip_archive::ManifestNode],
) -> Result<u64, ExtractError> {
    manifest
        .iter()
        .filter(|node| node.kind == crate::zip_archive::EntryKind::File)
        .try_fold(0u64, |total, node| {
            let size = node
                .declared_uncompressed_size
                .ok_or_else(|| ExtractError("manifest file has no declared size".into()))?;
            if size > MAX_ENTRY_BYTES {
                return Err(ExtractError("declared entry exceeds 4 GiB".into()));
            }
            total
                .checked_add(size)
                .filter(|value| *value <= MAX_TOTAL_BYTES)
                .ok_or_else(|| ExtractError("declared expanded size exceeds 8 GiB".into()))
        })
}

#[cfg(windows)]
fn materialize<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    manifest: &[ManifestNode],
    lease: super::windows::StagingRootLease,
    cancelled: &AtomicBool,
) -> Result<(super::windows::VerifiedStagedTree, u64), ExtractError> {
    use super::windows::{PinnedSourceHandle, StagedChildFact, VerifiedStagedNode};
    use sha2::Digest;
    let root_path = lease.root_path.clone();
    let root_identity = lease.root_identity.clone();
    let mut handles: Vec<VerifiedStagedNode> = Vec::with_capacity(manifest.len());
    let mut dirs: HashMap<Vec<String>, usize> = HashMap::new();
    let mut total = 0u64;
    let mut buf = vec![0u8; CHUNK];
    let built = (|| -> Result<(), ExtractError> {
        for node in manifest {
            if cancelled.load(AtomicOrdering::Acquire) {
                return Err(ExtractError("cancelled".into()));
            }
            let parent = if node.components.len() == 1 {
                &lease.root
            } else {
                let parent_path = node.components[..node.components.len() - 1].to_vec();
                let index = *dirs
                    .get(&parent_path)
                    .ok_or_else(|| ExtractError("staged parent directory missing".into()))?;
                match &handles[index].handle {
                    PinnedSourceHandle::Directory(directory) => directory,
                    _ => return Err(ExtractError("staged parent is not a directory".into())),
                }
            };
            let component = node
                .components
                .last()
                .unwrap()
                .encode_utf16()
                .collect::<Vec<_>>();
            let rel = node
                .components
                .iter()
                .map(|s| s.encode_utf16().collect())
                .collect::<Vec<Vec<u16>>>();
            if node.kind == NodeKind::Directory {
                let directory = super::windows::create_staged_directory(
                    parent, &component, true, 1,
                )
                .map_err(|e| ExtractError(format!("staged directory creation failed: {e:?}")))?;
                let identity =
                    super::windows::directory_identity_text(&directory).map_err(|e| {
                        ExtractError(format!("staged directory identity failed: {e:?}"))
                    })?;
                let index = handles.len();
                handles.push(VerifiedStagedNode {
                    relative_components: rel,
                    node_kind: node.kind,
                    identity,
                    content_fact: "directory".into(),
                    handle: PinnedSourceHandle::Directory(directory),
                });
                dirs.insert(node.components.clone(), index);
            } else {
                let out = super::windows::create_staged_file(parent, &component).map_err(|e| {
                    ExtractError(format!("exclusive staged file creation failed: {e:?}"))
                })?;
                let file_id = super::windows::regular_file_identity(&out)
                    .map_err(|e| ExtractError(format!("staged file identity failed: {e:?}")))?;
                let identity = format!(
                    "{}:{:08x}{:08x}",
                    file_id.volume, file_id.index_high, file_id.index_low
                );
                let staged_index = handles.len();
                handles.push(VerifiedStagedNode {
                    relative_components: rel,
                    node_kind: node.kind,
                    identity,
                    content_fact: String::new(),
                    handle: PinnedSourceHandle::File(out),
                });
                let idx = node
                    .entry_index
                    .ok_or_else(|| ExtractError("file node lacks archive entry".into()))?;
                let mut input = archive
                    .by_index(idx)
                    .map_err(|e| ExtractError(e.to_string()))?;
                if input.encrypted() {
                    return Err(ExtractError("encrypted ZIP entry".into()));
                }
                let mut hasher = sha2::Sha256::new();
                let declared = input.size();
                let mut entry_bytes = 0u64;
                loop {
                    if cancelled.load(AtomicOrdering::Acquire) {
                        return Err(ExtractError("cancelled".into()));
                    }
                    let n = input
                        .read(&mut buf)
                        .map_err(|e| ExtractError(format!("ZIP stream/CRC failure: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    entry_bytes = entry_bytes
                        .checked_add(n as u64)
                        .filter(|v| *v <= MAX_ENTRY_BYTES)
                        .ok_or_else(|| ExtractError("actual entry output exceeds 4 GiB".into()))?;
                    total = total
                        .checked_add(n as u64)
                        .filter(|v| *v <= MAX_TOTAL_BYTES)
                        .ok_or_else(|| {
                            ExtractError("actual expanded output exceeds 8 GiB".into())
                        })?;
                    match &mut handles[staged_index].handle {
                        PinnedSourceHandle::File(out) => out
                            .write_all(&buf[..n])
                            .map_err(|e| ExtractError(e.to_string()))?,
                        _ => return Err(ExtractError("staged file handle changed".into())),
                    };
                    hasher.update(&buf[..n]);
                }
                if entry_bytes != declared {
                    return Err(ExtractError(
                        "actual entry size differs from its declaration".into(),
                    ));
                }
                match &mut handles[staged_index].handle {
                    PinnedSourceHandle::File(out) => {
                        out.sync_all().map_err(|e| ExtractError(e.to_string()))?
                    }
                    _ => return Err(ExtractError("staged file handle changed".into())),
                };
                handles[staged_index].content_fact = format!("sha256:{:x}", hasher.finalize());
            }
        }
        Ok(())
    })();
    if let Err(error) = built {
        let cleanup = super::windows::cleanup_staged_handles(
            &lease.root,
            &root_identity,
            std::mem::take(&mut handles),
        );
        drop(lease.root);
        drop(lease.root_chain);
        return match cleanup {
            Ok(()) => Err(error),
            Err(e) => Err(ExtractError(format!(
                "partial:stage {} ({root_identity}) cleanup failed after {error}: {e:?}",
                root_path.display()
            ))),
        };
    }
    let facts = handles
        .iter()
        .map(|node| StagedChildFact {
            relative_components: node.relative_components.clone(),
            node_kind: node.node_kind,
            identity: node.identity.clone(),
            content_fact: node.content_fact.clone(),
        })
        .collect();
    let tree = super::windows::VerifiedStagedTree {
        root_path,
        root_identity,
        root_chain: lease.root_chain,
        root: lease.root,
        nodes: facts,
        handles,
    };
    Ok((tree, total))
}

#[cfg(windows)]
fn cleanup_stage(
    staged: super::windows::VerifiedStagedTree,
    path: &Path,
) -> Result<(), ExtractError> {
    let root = staged.root_identity.clone();
    super::windows::cleanup_staged_handles(&staged.root, &root, staged.handles).map_err(|e| {
        ExtractError(format!(
            "partial:prepublish cleanup failed; inspect {} ({e:?})",
            path.display()
        ))
    })?;
    drop(staged.root);
    drop(staged.root_chain);
    Ok(())
}

#[cfg(windows)]
fn clean_stage_failure(
    lease: super::windows::StagingRootLease,
    nodes: Vec<super::windows::VerifiedStagedNode>,
    path: &Path,
    original: ExtractError,
) -> ExtractError {
    let root = lease.root_identity.clone();
    match super::windows::cleanup_staged_handles(&lease.root, &root, nodes) {
        Ok(()) => ExtractError(original.0),
        Err(e) => ExtractError(format!(
            "partial:stage {} ({root}) cleanup failed after {original}: {e:?}",
            path.display()
        )),
    }
}

fn journal_ledger(
    token: AttemptToken,
    item: ItemId,
    ledger: &JournalZipExtract,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) -> Result<(), LedgerError> {
    emit(Fact::ZipExtractLedgerIntent {
        token,
        item,
        ledger: ledger.clone(),
    });
    let mut cancelled_before_planned_ack = false;
    loop {
        match controls.recv() {
            Ok(WorkerControl::ZipExtractLedgerJournaledExact {
                token: ack,
                item: ack_item,
                ledger: saved,
            }) if ack == token && ack_item == item && saved == *ledger => {
                return if cancelled_before_planned_ack {
                    Err(LedgerError::Cancelled)
                } else {
                    Ok(())
                };
            }
            Ok(WorkerControl::ZipExtractLedgerJournalFailedExact {
                token: ack,
                item: ack_item,
                ledger: failed,
            }) if ack == token && ack_item == item && failed == *ledger => {
                return Err(LedgerError::AcknowledgementFailed);
            }
            Ok(WorkerControl::Cancel)
                if ledger.phase == Phase::StageDiscarded || ledger.phase == Phase::StagePlanned =>
            {
                cancelled_before_planned_ack |= ledger.phase == Phase::StagePlanned;
            }
            Ok(WorkerControl::Cancel) => return Err(LedgerError::Cancelled),
            _ => {
                return Err(LedgerError::AcknowledgementFailed);
            }
        }
    }
}

fn unique_stage_path(parent: &Path, token: AttemptToken) -> Result<PathBuf, ExtractError> {
    for _ in 0..16 {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| ExtractError(e.to_string()))?;
        let name = format!(
            ".gpui-filer-extract-{}-{}-{:02x}{:02x}{:02x}{:02x}.stage",
            token.job_id().value(),
            token.attempt_id().value(),
            nonce[0],
            nonce[1],
            nonce[2],
            nonce[3]
        );
        let path = parent.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(ExtractError(
        "could not reserve a unique staging name".into(),
    ))
}

#[cfg(windows)]
fn open_source(path: &Path) -> Result<File, ExtractError> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.read(true).share_mode(1).custom_flags(0x0020_0000);
    options.open(path).map_err(|e| {
        ExtractError(format!(
            "could not open source ZIP without following links: {e}"
        ))
    })
}

#[cfg(not(windows))]
fn open_source(path: &Path) -> Result<File, ExtractError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| ExtractError(e.to_string()))
}

#[cfg(windows)]
fn verify_source(
    path: &Path,
    file: &File,
    expected: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<(), ExtractError> {
    let current = crate::directory::EntrySourceIdentity::from_path_no_follow(
        path,
        crate::directory::EntryKind::File,
    )
    .ok_or_else(|| ExtractError("source identity changed".into()))?;
    if current.filesystem_identity() != *expected {
        return Err(ExtractError("source identity changed".into()));
    }
    let actual = super::windows::regular_file_identity(file)
        .map_err(|e| ExtractError(format!("source handle identity failed: {e:?}")))?;
    if expected.windows_parts()
        != Some((
            actual.volume,
            ((actual.index_high as u64) << 32) | actual.index_low as u64,
        ))
    {
        return Err(ExtractError("source handle identity changed".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_token() -> AttemptToken {
        let request = crate::operations::model::OperationRequest {
            kind: crate::operations::model::OperationKind::ExtractZip,
            virtual_source: None,
            sources: Vec::new(),
            destination: None,
            batch_rename_plan: None,
            move_identity_guard: None,
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
            trash_source_guards: None,
            source_cleanup_identity_guard: None,
            initial_conflict_policy: None,
            copy_destination_identity_guard: None,
            create_parent_directory_identity_guard: None,
            zip_create_guard: None,
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        };
        crate::operations::model::OperationService::default()
            .submit(request)
            .unwrap()
    }

    fn test_ledger(phase: Phase) -> JournalZipExtract {
        let root = std::env::temp_dir().join("gpui-filer-zip-extract-ledger-test");
        JournalZipExtract {
            source: JournalPath::from_path(&root.join("source.zip")),
            output: JournalPath::from_path(&root.join("output")),
            stage: JournalPath::from_path(&root.join("stage")),
            source_identity: "source-id".into(),
            parent_identity: "parent-id".into(),
            stage_identity: Some("stage-id".into()),
            phase,
        }
    }

    #[test]
    fn prepublish_cleanup_failure_keeps_recovery_identity_and_partial_outcome() {
        let path = std::env::temp_dir().join("gpui-filer-stage");
        let error = before_publish_cleanup_unconfirmed(
            super::super::CopyError::PrePublishPartial("cleanup failed".into()),
            &path,
            Some("stage-id"),
        );
        assert!(error.0.starts_with("partial:"));
        assert!(error.0.contains(&path.display().to_string()));
        assert!(error.0.contains("stage-id"));
        assert_eq!(
            test_ledger(Phase::PublishIntent).phase,
            Phase::PublishIntent
        );
    }

    #[test]
    fn cancellation_at_durable_barrier_is_typed_and_discard_ack_still_requires_exact_match() {
        use std::sync::mpsc;

        let token = test_token();
        let ledger = test_ledger(Phase::PublishIntent);
        let (sender, receiver) = mpsc::channel();
        sender.send(WorkerControl::Cancel).unwrap();
        let mut emitted = Vec::new();
        let result = journal_ledger(token, ItemId(0), &ledger, &receiver, &mut |fact| {
            emitted.push(fact)
        });
        assert_eq!(result, Err(LedgerError::Cancelled));
        assert_eq!(ExtractError::from(result.unwrap_err()).0, "cancelled");
        assert_eq!(emitted.len(), 1);

        let mut discarded = ledger;
        discarded.phase = Phase::StageDiscarded;
        let (sender, receiver) = mpsc::channel();
        sender.send(WorkerControl::Cancel).unwrap();
        sender
            .send(WorkerControl::ZipExtractLedgerJournaledExact {
                token,
                item: ItemId(0),
                ledger: discarded.clone(),
            })
            .unwrap();
        assert!(journal_ledger(token, ItemId(0), &discarded, &receiver, &mut |_| {}).is_ok());
    }

    #[test]
    fn cancel_before_first_planned_ack_waits_for_exact_reply_then_returns_cancelled() {
        use std::sync::mpsc;

        let token = test_token();
        let mut planned = test_ledger(Phase::StagePlanned);
        planned.stage_identity = None;
        let (sender, receiver) = mpsc::channel();
        sender.send(WorkerControl::Cancel).unwrap();
        sender
            .send(WorkerControl::ZipExtractLedgerJournaledExact {
                token,
                item: ItemId(0),
                ledger: planned.clone(),
            })
            .unwrap();
        let mut emitted = Vec::new();
        assert_eq!(
            journal_ledger(token, ItemId(0), &planned, &receiver, &mut |fact| emitted
                .push(fact)),
            Err(LedgerError::Cancelled)
        );
        assert_eq!(emitted.len(), 1);
        assert!(receiver.try_recv().is_err());
    }

    fn parse_entries(names: &[(&str, bool, u64)]) -> Result<Vec<ManifestNode>, ExtractError> {
        let mut nodes = Vec::new();
        for (index, (name, is_dir, size)) in names.iter().enumerate() {
            let clean = if name.ends_with('/') {
                &name[..name.len() - 1]
            } else {
                name
            };
            if clean.is_empty()
                || clean.starts_with('/')
                || clean.contains('\\')
                || clean.ends_with('/')
            {
                return Err(ExtractError("bad path".into()));
            }
            let components = clean.split('/').map(str::to_owned).collect::<Vec<_>>();
            if components.len() > MAX_DEPTH
                || components
                    .iter()
                    .any(|p| !crate::operations::rename::is_single_name(p))
            {
                return Err(ExtractError("unsafe path".into()));
            }
            if *size > MAX_ENTRY_BYTES {
                return Err(ExtractError("large".into()));
            }
            for depth in 1..=components.len() {
                let leaf = depth == components.len();
                nodes.push(ManifestNode {
                    components: components[..depth].to_vec(),
                    kind: if !leaf || *is_dir {
                        NodeKind::Directory
                    } else {
                        NodeKind::File
                    },
                    explicit: leaf,
                    entry_index: (leaf && !*is_dir).then_some(index),
                });
            }
        }
        nodes.sort_by(|a, b| compare_paths(&a.components, &b.components));
        let mut out = Vec::<ManifestNode>::new();
        for n in nodes {
            if let Some(p) = out
                .last_mut()
                .filter(|p| compare_paths(&p.components, &n.components) == Ordering::Equal)
            {
                if p.components != n.components || p.kind != n.kind || (p.explicit && n.explicit) {
                    return Err(ExtractError("collision".into()));
                }
                if n.explicit {
                    p.explicit = true;
                    p.entry_index = n.entry_index;
                }
            } else {
                out.push(n);
            }
        }
        for n in &out {
            for d in 1..n.components.len() {
                let p = &n.components[..d];
                let i = out
                    .binary_search_by(|v| compare_paths(&v.components, p))
                    .map_err(|_| ExtractError("parent".into()))?;
                if out[i].kind != NodeKind::Directory {
                    return Err(ExtractError("conflict".into()));
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn extraction_declared_size_policy_accepts_exact_limits_and_rejects_overflow() {
        use crate::zip_archive::{EntryKind, ManifestNode as ArchiveNode};

        let file = |index, size| ArchiveNode {
            components: vec![format!("file-{index}.bin")],
            kind: EntryKind::File,
            explicit: true,
            entry_index: Some(index),
            declared_uncompressed_size: Some(size),
        };
        let directory = ArchiveNode {
            components: vec!["empty".into()],
            kind: EntryKind::Directory,
            explicit: true,
            entry_index: None,
            declared_uncompressed_size: None,
        };

        assert_eq!(
            validate_declared_total(&[file(0, MAX_ENTRY_BYTES)]).unwrap(),
            MAX_ENTRY_BYTES
        );
        assert_eq!(
            validate_declared_total(&[
                file(0, MAX_ENTRY_BYTES),
                file(1, MAX_ENTRY_BYTES),
                directory
            ])
            .unwrap(),
            MAX_TOTAL_BYTES
        );
        assert!(validate_declared_total(&[file(0, MAX_ENTRY_BYTES + 1)]).is_err());
        assert!(
            validate_declared_total(&[
                file(0, MAX_ENTRY_BYTES),
                file(1, MAX_ENTRY_BYTES),
                file(2, 1),
            ])
            .is_err()
        );
    }
    #[test]
    fn bounded_manifest_synthesizes_parents_and_accepts_empty() {
        assert_eq!(parse_entries(&[]).unwrap().len(), 0);
        let m = parse_entries(&[("a/b.txt", false, 4)]).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].kind, NodeKind::Directory);
    }
    #[test]
    fn manifest_rejects_traversal_aliases_duplicates_and_conflicts() {
        for name in [
            "../x",
            "/x",
            "C:/x",
            "\\\\server\\x",
            "a//b",
            "a/./b",
            "a/../b",
            "a\\b",
            "CON.txt",
            "name.",
            "name ",
            "a:b",
        ] {
            assert!(parse_entries(&[(name, false, 1)]).is_err(), "{name}");
        }
        assert!(parse_entries(&[("A/x", false, 1), ("a/y", false, 1)]).is_err());
        assert!(parse_entries(&[("a/", true, 0), ("a/", true, 0)]).is_err());
        assert!(parse_entries(&[("a", false, 1), ("a/b", false, 1)]).is_err());
    }
    #[test]
    fn rejects_multidisk_zip_and_zip64_markers() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("e.zip");
        std::fs::write(&path, b"not zip").unwrap();
        let mut f = File::open(path).unwrap();
        assert!(scan_central_directory(&mut f, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn resolves_only_zip64_sentinel_fields_in_spec_order() {
        let mut extra = Vec::new();
        extra.extend_from_slice(&1u16.to_le_bytes());
        extra.extend_from_slice(&20u16.to_le_bytes());
        extra.extend_from_slice(&11u64.to_le_bytes());
        extra.extend_from_slice(&22u64.to_le_bytes());
        extra.extend_from_slice(&33u32.to_le_bytes());
        assert_eq!(
            resolve_zip64_extra(
                &extra,
                true,
                true,
                false,
                true,
                u32::MAX as u64,
                u32::MAX as u64,
                7,
                u16::MAX as u32
            )
            .unwrap(),
            (11, 22, 7, 33)
        );
        assert!(
            resolve_zip64_extra(
                &extra,
                true,
                true,
                true,
                false,
                u32::MAX as u64,
                u32::MAX as u64,
                u32::MAX as u64,
                0
            )
            .is_err()
        );
        let missing = [1u16.to_le_bytes(), 0u16.to_le_bytes()].concat();
        assert!(
            resolve_zip64_extra(
                &missing,
                true,
                false,
                false,
                false,
                u32::MAX as u64,
                0,
                0,
                0
            )
            .is_err()
        );
        let duplicate = [extra.as_slice(), extra.as_slice()].concat();
        assert!(resolve_zip64_extra(&duplicate, true, true, false, true, 0, 0, 7, 0).is_err());
        let mut surplus = extra.clone();
        surplus[2..4].copy_from_slice(&21u16.to_le_bytes());
        surplus.push(0);
        assert!(resolve_zip64_extra(&surplus, true, true, false, true, 0, 0, 7, 0).is_err());
        let nonzero_disk = [
            1u16.to_le_bytes().as_slice(),
            12u16.to_le_bytes().as_slice(),
            11u64.to_le_bytes().as_slice(),
            1u32.to_le_bytes().as_slice(),
        ]
        .concat();
        assert_eq!(
            resolve_zip64_extra(
                &nonzero_disk,
                true,
                false,
                false,
                true,
                u32::MAX as u64,
                0,
                0,
                u16::MAX as u32
            )
            .unwrap()
            .3,
            1
        );
        assert!(
            resolve_zip64_extra(
                &extra[..extra.len() - 1],
                true,
                true,
                false,
                true,
                0,
                0,
                7,
                0
            )
            .is_err()
        );
    }

    #[test]
    fn validated_reader_accepts_unprefixed_classic_metadata_and_then_data() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("trace.zip");
        {
            let file = File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("item.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"payload").unwrap();
            writer.finish().unwrap();
        }
        let mut file = File::open(&path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        let mode = Cell::new(false);
        let reader =
            validated_reader::Reader::new(file.try_clone().unwrap(), scanned.layout.clone(), &mode);
        let mut archive = zip::ZipArchive::with_config(
            zip::read::Config {
                archive_offset: zip::read::ArchiveOffset::Known(0),
            },
            reader,
        )
        .unwrap();
        assert_eq!(archive.offset(), 0);
        assert_eq!(archive.central_directory_start(), scanned.cd_start);
        assert_eq!(archive.len(), 1);
        mode.set(true);
        assert!(build_manifest_from_archive(&mut archive, &scanned.entries).is_ok());
        let mut entry = archive.by_index(0).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"payload");
    }

    #[test]
    fn bootstrap_start_with_approved_header_reads_from_the_synchronized_offset() {
        let mode = Cell::new(false);
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"PK\x03\x04");
        let mut source = std::io::Cursor::new(bytes);
        source.set_position(11);
        let layout = validated_reader::Layout {
            logical_eof: 64,
            eocd_start: 42,
            eocd: [0; 22],
            segments: vec![validated_reader::ReadSegment { start: 0, end: 30 }],
            seek_points: vec![validated_reader::SeekPoint {
                offset: 0,
                first_read_cap: 30,
            }],
        };
        let mut reader = validated_reader::Reader::new(source, layout, &mode);
        assert_eq!(reader.seek(SeekFrom::Start(0)).unwrap(), 0);
        let mut signature = [0; 4];
        reader.read_exact(&mut signature).unwrap();
        assert_eq!(&signature, b"PK\x03\x04");
    }

    #[test]
    fn accepts_empty_classic_archive_at_offset_zero() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("empty.zip");
        zip::ZipWriter::new(File::create(&path).unwrap())
            .finish()
            .unwrap();
        let mut file = File::open(&path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        assert!(scanned.entries.is_empty());
        assert_eq!(scanned.cd_start, 0);
        let mode = Cell::new(false);
        let reader = validated_reader::Reader::new(file, scanned.layout.clone(), &mode);
        let archive = zip::ZipArchive::with_config(
            zip::read::Config {
                archive_offset: zip::read::ArchiveOffset::Known(0),
            },
            reader,
        )
        .unwrap();
        assert_eq!(archive.offset(), 0);
        assert_eq!(archive.central_directory_start(), 0);
        assert!(archive.is_empty());
    }

    #[test]
    fn accepts_empty_zip64_footer_with_matching_classic_zero_fields() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("empty64.zip");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"PK\x06\x06");
        bytes.extend_from_slice(&44u64.to_le_bytes());
        bytes.extend_from_slice(&45u16.to_le_bytes());
        bytes.extend_from_slice(&45u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(b"PK\x06\x07");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let mut file = File::open(&path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        assert!(scanned.entries.is_empty());
        assert_eq!(scanned.cd_start, 0);
        let mode = Cell::new(false);
        let reader =
            validated_reader::Reader::new(file.try_clone().unwrap(), scanned.layout.clone(), &mode);
        let archive = zip::ZipArchive::with_config(
            zip::read::Config {
                archive_offset: zip::read::ArchiveOffset::Known(0),
            },
            reader,
        )
        .unwrap();
        assert_eq!(archive.offset(), 0);
        assert_eq!(archive.central_directory_start(), 0);
        assert!(archive.is_empty());
        mode.set(true);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn rejects_malformed_zip64_footer_fields_before_parser_construction() {
        fn empty_zip64() -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"PK\x06\x06");
            bytes.extend_from_slice(&44u64.to_le_bytes());
            bytes.extend_from_slice(&45u16.to_le_bytes());
            bytes.extend_from_slice(&45u16.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(b"PK\x06\x07");
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(b"PK\x05\x06");
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes
        }
        let root = tempfile::tempdir().unwrap();
        for (name, mutate) in [("disk", 0usize), ("cd-offset", 2), ("locator-offset", 3)] {
            let mut bytes = empty_zip64();
            match mutate {
                0 => bytes[16..20].copy_from_slice(&1u32.to_le_bytes()),
                2 => bytes[48..56].copy_from_slice(&1u64.to_le_bytes()),
                3 => bytes[64..72].copy_from_slice(&u64::MAX.to_le_bytes()),
                _ => unreachable!(),
            }
            let path = root.path().join(format!("{name}.zip"));
            std::fs::write(&path, bytes).unwrap();
            let mut file = File::open(path).unwrap();
            assert!(
                scan_central_directory(&mut file, &AtomicBool::new(false)).is_err(),
                "{name}"
            );
        }

        let mut oversized_count = empty_zip64();
        oversized_count[24..32].copy_from_slice(&((MAX_ENTRIES as u64) + 1).to_le_bytes());
        oversized_count[32..40].copy_from_slice(&((MAX_ENTRIES as u64) + 1).to_le_bytes());
        oversized_count[84..86].copy_from_slice(&u16::MAX.to_le_bytes());
        oversized_count[86..88].copy_from_slice(&u16::MAX.to_le_bytes());
        let count_path = root.path().join("count-limit.zip");
        std::fs::write(&count_path, oversized_count).unwrap();
        let mut file = File::open(count_path).unwrap();
        let error = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap_err();
        assert!(error.0.contains("configured bounds"), "{error:?}");

        let oversized_cd_size = MAX_CENTRAL_BYTES + 1;
        let mut cd_tail = Vec::new();
        cd_tail.extend_from_slice(b"PK\x06\x06");
        cd_tail.extend_from_slice(&44u64.to_le_bytes());
        cd_tail.extend_from_slice(&45u16.to_le_bytes());
        cd_tail.extend_from_slice(&45u16.to_le_bytes());
        cd_tail.extend_from_slice(&0u32.to_le_bytes());
        cd_tail.extend_from_slice(&0u32.to_le_bytes());
        cd_tail.extend_from_slice(&0u64.to_le_bytes());
        cd_tail.extend_from_slice(&0u64.to_le_bytes());
        cd_tail.extend_from_slice(&oversized_cd_size.to_le_bytes());
        cd_tail.extend_from_slice(&0u64.to_le_bytes());
        cd_tail.extend_from_slice(b"PK\x06\x07");
        cd_tail.extend_from_slice(&0u32.to_le_bytes());
        cd_tail.extend_from_slice(&oversized_cd_size.to_le_bytes());
        cd_tail.extend_from_slice(&1u32.to_le_bytes());
        cd_tail.extend_from_slice(b"PK\x05\x06");
        cd_tail.extend_from_slice(&[0; 18]);
        cd_tail[88..92].copy_from_slice(&u32::MAX.to_le_bytes());
        cd_tail[92..96].copy_from_slice(&u32::MAX.to_le_bytes());
        let cd_path = root.path().join("central-size-limit.zip");
        let mut cd_file = File::create(&cd_path).unwrap();
        cd_file
            .set_len(oversized_cd_size + cd_tail.len() as u64)
            .unwrap();
        cd_file.seek(SeekFrom::Start(oversized_cd_size)).unwrap();
        cd_file.write_all(&cd_tail).unwrap();
        drop(cd_file);
        let mut file = File::open(cd_path).unwrap();
        let error = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap_err();
        assert!(error.0.contains("configured bounds"), "{error:?}");

        let extension_size = MAX_ZIP64_EXTENSION + 1;
        let mut extension_bytes = empty_zip64();
        let eocd_start = 56 + extension_size as usize;
        extension_bytes.truncate(56);
        extension_bytes[4..12].copy_from_slice(&(44 + extension_size).to_le_bytes());
        extension_bytes.resize(eocd_start, 0);
        extension_bytes.extend_from_slice(b"PK\x06\x07");
        extension_bytes.extend_from_slice(&0u32.to_le_bytes());
        extension_bytes.extend_from_slice(&0u64.to_le_bytes());
        extension_bytes.extend_from_slice(&1u32.to_le_bytes());
        extension_bytes.extend_from_slice(b"PK\x05\x06");
        extension_bytes.extend_from_slice(&[0; 18]);
        let extension_path = root.path().join("extension-limit.zip");
        std::fs::write(&extension_path, extension_bytes).unwrap();
        let mut file = File::open(extension_path).unwrap();
        let error = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap_err();
        assert!(error.0.contains("footer extent/version"), "{error:?}");
    }

    #[test]
    fn short_malformed_zip_is_rejected_without_panicking() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("short.zip");
        std::fs::write(&path, [0u8; 21]).unwrap();
        let mut file = File::open(path).unwrap();
        assert!(scan_central_directory(&mut file, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn accepts_forced_small_zip64_entry_with_classic_footer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("forced.zip");
        {
            let file = File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default().large_file(true);
            writer.start_file("nested/item.txt", options).unwrap();
            writer.write_all(b"small payload").unwrap();
            writer.finish().unwrap();
        }
        let mut file = File::open(&path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        assert_eq!(scanned.entries.len(), 1);
        assert_eq!(scanned.entries[0].uncompressed, 13);
        let mode = Cell::new(false);
        let reader =
            validated_reader::Reader::new(file.try_clone().unwrap(), scanned.layout.clone(), &mode);
        let mut archive = zip::ZipArchive::with_config(
            zip::read::Config {
                archive_offset: zip::read::ArchiveOffset::Known(0),
            },
            reader,
        )
        .unwrap();
        assert_eq!(archive.len(), 1);
        mode.set(true);
        assert!(build_manifest_from_archive(&mut archive, &scanned.entries).is_ok());
        let mut entry = archive.by_index(0).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"small payload");
    }

    #[test]
    fn rejects_zip64_compressed_range_overflow_but_inspects_large_declared_size() {
        fn forced_entry() -> Vec<u8> {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            writer
                .start_file(
                    "x",
                    zip::write::SimpleFileOptions::default().large_file(true),
                )
                .unwrap();
            writer.write_all(b"x").unwrap();
            writer.finish().unwrap().into_inner()
        }

        let root = tempfile::tempdir().unwrap();
        let original = forced_entry();
        let cd = original
            .windows(4)
            .rposition(|window| window == b"PK\x01\x02")
            .unwrap();
        let name_len = u16::from_le_bytes([original[cd + 28], original[cd + 29]]) as usize;
        let extra = cd + 46 + name_len;
        assert_eq!(&original[extra..extra + 2], &1u16.to_le_bytes());

        let mut compressed_overflow = original.clone();
        compressed_overflow[extra + 4 + 8..extra + 12 + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let overflow_path = root.path().join("compressed-overflow.zip");
        std::fs::write(&overflow_path, compressed_overflow).unwrap();
        let mut file = File::open(overflow_path).unwrap();
        assert!(scan_central_directory(&mut file, &AtomicBool::new(false)).is_err());

        let mut oversized_metadata = original;
        oversized_metadata[extra + 4..extra + 12]
            .copy_from_slice(&(MAX_ENTRY_BYTES + 1).to_le_bytes());
        let oversized_path = root.path().join("expanded-limit.zip");
        std::fs::write(&oversized_path, oversized_metadata).unwrap();
        let mut file = File::open(&oversized_path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        assert_eq!(scanned.entries[0].uncompressed, MAX_ENTRY_BYTES + 1);
        let inspected = crate::zip_archive::inspect(
            &mut File::open(oversized_path).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            inspected.nodes[0].declared_uncompressed_size,
            Some(MAX_ENTRY_BYTES + 1)
        );
    }

    #[test]
    fn accepts_zip64_footer_with_maximum_eocd_comment() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("footer.zip");
        {
            let file = File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("a.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"a").unwrap();
            writer.finish().unwrap();
        }
        let classic = std::fs::read(&path).unwrap();
        let eocd_offset = classic
            .windows(4)
            .rposition(|window| window == b"PK\x05\x06")
            .unwrap();
        let classic_eocd: [u8; 22] = classic[eocd_offset..eocd_offset + 22].try_into().unwrap();
        let count = u64::from(u16::from_le_bytes([classic_eocd[10], classic_eocd[11]]));
        let cd_size = u64::from_le_bytes([
            classic_eocd[12],
            classic_eocd[13],
            classic_eocd[14],
            classic_eocd[15],
            0,
            0,
            0,
            0,
        ]);
        let cd_offset = u64::from_le_bytes([
            classic_eocd[16],
            classic_eocd[17],
            classic_eocd[18],
            classic_eocd[19],
            0,
            0,
            0,
            0,
        ]);
        let zip64_offset = eocd_offset as u64;
        let mut footer = Vec::with_capacity(76);
        footer.extend_from_slice(b"PK\x06\x06");
        footer.extend_from_slice(&44u64.to_le_bytes());
        footer.extend_from_slice(&45u16.to_le_bytes());
        footer.extend_from_slice(&45u16.to_le_bytes());
        footer.extend_from_slice(&0u32.to_le_bytes());
        footer.extend_from_slice(&0u32.to_le_bytes());
        footer.extend_from_slice(&count.to_le_bytes());
        footer.extend_from_slice(&count.to_le_bytes());
        footer.extend_from_slice(&cd_size.to_le_bytes());
        footer.extend_from_slice(&cd_offset.to_le_bytes());
        footer.extend_from_slice(b"PK\x06\x07");
        footer.extend_from_slice(&0u32.to_le_bytes());
        footer.extend_from_slice(&zip64_offset.to_le_bytes());
        footer.extend_from_slice(&1u32.to_le_bytes());
        let mut eocd = classic_eocd;
        eocd[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
        eocd[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        eocd[20..22].copy_from_slice(&u16::MAX.to_le_bytes());
        for (label, extension_size) in
            [("small", 10usize), ("at-cap", MAX_ZIP64_EXTENSION as usize)]
        {
            let payload_size = extension_size - 6;
            let mut extension = Vec::with_capacity(extension_size);
            extension.extend_from_slice(&0xcafeu16.to_le_bytes());
            extension.extend_from_slice(&(payload_size as u32).to_le_bytes());
            extension.resize(extension_size, 0x5a);
            let mut footer_with_extension = footer.clone();
            footer_with_extension.splice(56..56, extension);
            footer_with_extension[4..12]
                .copy_from_slice(&(44 + extension_size as u64).to_le_bytes());

            let mut archive_bytes = classic[..eocd_offset].to_vec();
            archive_bytes.extend_from_slice(&footer_with_extension);
            archive_bytes.extend_from_slice(&eocd);
            archive_bytes.resize(archive_bytes.len() + u16::MAX as usize, b'x');
            let original_archive_bytes = archive_bytes.clone();
            let case_path = root.path().join(format!("footer-{label}.zip"));
            std::fs::write(&case_path, archive_bytes).unwrap();

            let mut file = File::open(&case_path).unwrap();
            let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
            assert_eq!(scanned.entries.len(), 1);
            let mode = Cell::new(false);
            let reader = validated_reader::Reader::new(
                file.try_clone().unwrap(),
                scanned.layout.clone(),
                &mode,
            );
            let mut archive = zip::ZipArchive::with_config(
                zip::read::Config {
                    archive_offset: zip::read::ArchiveOffset::Known(0),
                },
                reader,
            )
            .unwrap();
            assert_eq!(archive.offset(), 0);
            assert_eq!(archive.central_directory_start(), scanned.cd_start);
            assert_eq!(archive.len(), 1);
            mode.set(true);
            assert!(build_manifest_from_archive(&mut archive, &scanned.entries).is_ok());
            let mut entry = archive.by_index(0).unwrap();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            assert_eq!(content, b"a");
            assert_eq!(std::fs::read(&case_path).unwrap(), original_archive_bytes);
        }
    }

    #[test]
    fn metadata_gate_rejects_aligned_fallback_window_and_synthesizes_bootstrap() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("gate.zip");
        {
            let file = File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("item.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"payload").unwrap();
            writer.finish().unwrap();
        }
        let mut file = File::open(path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        let mode = Cell::new(false);
        let mut reader =
            validated_reader::Reader::new(file.try_clone().unwrap(), scanned.layout.clone(), &mode);
        reader.seek(SeekFrom::Start(scanned.cd_start)).unwrap();
        let mut fallback_window = [0u8; 2048];
        assert!(reader.read(&mut fallback_window).is_err());

        let mode = Cell::new(false);
        let mut reader = validated_reader::Reader::new(file, scanned.layout.clone(), &mode);
        let bootstrap_start = scanned.layout.logical_eof.min(2048);
        let bootstrap_start = scanned.layout.logical_eof - bootstrap_start;
        reader.seek(SeekFrom::Start(bootstrap_start)).unwrap();
        let mut boot = Vec::new();
        let mut chunk = [0u8; 64];
        loop {
            let count = reader.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            boot.extend_from_slice(&chunk[..count]);
        }
        assert_eq!(
            boot.len() as u64,
            scanned.layout.logical_eof - bootstrap_start
        );
        let relative_eocd = (scanned.layout.eocd_start - bootstrap_start) as usize;
        assert_eq!(&boot[relative_eocd..relative_eocd + 4], b"PK\x05\x06");
        assert_eq!(&boot[relative_eocd + 20..relative_eocd + 22], &[0, 0]);
        reader.seek(SeekFrom::Start(bootstrap_start)).unwrap();
        assert!(reader.read(&mut [0u8; 2048]).is_err());
    }

    fn assert_malformed_entry_fallback_is_blocked(comment_size: Option<usize>) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fallback.zip");
        {
            let file = File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("x", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"x").unwrap();
            writer.finish().unwrap();
        }
        let original = std::fs::read(&path).unwrap();
        let eocd = original
            .windows(4)
            .rposition(|window| window == b"PK\x05\x06")
            .unwrap();
        let eocd_header: [u8; 22] = original[eocd..eocd + 22].try_into().unwrap();
        let cd_start = u32::from_le_bytes(eocd_header[16..20].try_into().unwrap()) as usize;
        let record = &original[cd_start..eocd];
        let name_len = u16::from_le_bytes([record[28], record[29]]) as usize;
        let extra_len = u16::from_le_bytes([record[30], record[31]]) as usize;
        let existing_comment_len = u16::from_le_bytes([record[32], record[33]]) as usize;
        let new_extra_len = extra_len + 5;
        let comment_len = comment_size.unwrap_or_else(|| 2048usize - 46 - name_len - new_extra_len);
        let target_cd_size = 46 + name_len + new_extra_len + comment_len;
        assert!(comment_len > 22);
        let mut central = record[..46 + name_len].to_vec();
        central[30..32].copy_from_slice(&(new_extra_len as u16).to_le_bytes());
        central[32..34].copy_from_slice(&(comment_len as u16).to_le_bytes());
        central.extend_from_slice(&record[46 + name_len..46 + name_len + extra_len]);
        central.extend_from_slice(&0x000au16.to_le_bytes());
        central.extend_from_slice(&1u16.to_le_bytes());
        central.push(0); // NTFS field with invalid length makes current metadata parsing fail.
        let comment_start = central.len();
        central.resize(target_cd_size, b'q');
        central[comment_start..comment_start + 22].fill(0);
        central[comment_start..comment_start + 4].copy_from_slice(b"PK\x05\x06");
        assert_eq!(central.len(), target_cd_size);
        assert_eq!(existing_comment_len, 0);
        let mut new_eocd = eocd_header;
        new_eocd[12..16].copy_from_slice(&(target_cd_size as u32).to_le_bytes());
        let mut bytes = original[..cd_start].to_vec();
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&new_eocd);
        std::fs::write(&path, bytes).unwrap();

        let mut file = File::open(&path).unwrap();
        let scanned = scan_central_directory(&mut file, &AtomicBool::new(false)).unwrap();
        assert_eq!(scanned.cd_start, cd_start as u64);
        let mode = Cell::new(false);
        let reader = validated_reader::Reader::new(file, scanned.layout.clone(), &mode);
        let result = zip::ZipArchive::with_config(
            zip::read::Config {
                archive_offset: zip::read::ArchiveOffset::Known(0),
            },
            reader,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("malformed metadata must be rejected"),
        };
        if comment_size.is_some() {
            assert!(
                error.to_string().contains("unapproved metadata seek"),
                "{error}"
            );
        } else {
            assert!(
                error.to_string().contains("oversized first metadata read"),
                "{error}"
            );
        }
    }

    #[test]
    fn malformed_current_entry_cannot_trigger_aligned_older_eocd_scan() {
        assert_malformed_entry_fallback_is_blocked(None);
    }

    #[test]
    fn malformed_current_entry_cannot_seek_to_variable_comment_fallback_window() {
        assert_malformed_entry_fallback_is_blocked(Some(2048));
    }

    #[cfg(windows)]
    #[test]
    fn worker_extracts_real_archive_and_preserves_existing_output_on_collision() {
        use std::sync::{Mutex, mpsc};

        fn run(request: OperationRequest) -> TerminalReport {
            let token = crate::operations::model::OperationService::default()
                .submit(request.clone())
                .unwrap();
            let (control_tx, control_rx) = mpsc::channel();
            let report = Mutex::new(None);
            execute(
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
                    Fact::Terminal {
                        report: terminal, ..
                    } => {
                        *report.lock().unwrap() = Some(terminal);
                    }
                    _ => {}
                },
            );
            report.into_inner().unwrap().expect("worker terminal fact")
        }

        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let archive_path = temp.path().join("sample.zip");
        let mut archive = zip::ZipWriter::new(File::create(&archive_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        archive.add_directory("nested/", options).unwrap();
        archive.add_directory("empty/", options).unwrap();
        archive.start_file("nested/payload.txt", options).unwrap();
        archive.write_all(b"actual archive payload\0bytes").unwrap();
        archive.finish().unwrap();
        let output_path = temp.path().join("sample");
        let guard = crate::operations::model::test_zip_create_guard(&archive_path, &output_path);
        let request =
            OperationRequest::extract_zip(archive_path.clone(), output_path.clone(), guard.clone())
                .unwrap();
        let report = run(request);
        assert_eq!(
            report.outcome,
            TerminalOutcome::Succeeded,
            "{:?}",
            report.receipts[0].detail
        );
        assert_eq!(report.receipts[0].outcome, ItemOutcome::Completed);
        assert_eq!(
            std::fs::read(output_path.join("nested/payload.txt")).unwrap(),
            b"actual archive payload\0bytes"
        );
        assert!(output_path.join("nested").is_dir());
        assert!(output_path.join("empty").is_dir());

        let collision_sentinel = output_path.join("keep.txt");
        std::fs::write(&collision_sentinel, b"must survive").unwrap();
        let stage_count = || {
            std::fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".gpui-filer-extract-")
                })
                .count()
        };
        let stages_before_collision = stage_count();
        let collision_request =
            OperationRequest::extract_zip(archive_path, output_path.clone(), guard).unwrap();
        let collision = run(collision_request);
        assert_eq!(
            collision.outcome,
            TerminalOutcome::Failed,
            "{:?}",
            collision.receipts[0].detail
        );
        assert_eq!(collision.receipts[0].outcome, ItemOutcome::Failed);
        assert_eq!(std::fs::read(collision_sentinel).unwrap(), b"must survive");
        assert_eq!(stage_count(), stages_before_collision);
        assert_eq!(
            std::fs::read(output_path.join("nested/payload.txt")).unwrap(),
            b"actual archive payload\0bytes"
        );

        let zip64_archive_path = temp.path().join("forced-small.zip");
        let mut zip64_writer = zip::ZipWriter::new(File::create(&zip64_archive_path).unwrap());
        zip64_writer.add_directory("nested/", options).unwrap();
        zip64_writer.add_directory("empty/", options).unwrap();
        zip64_writer
            .start_file(
                "nested/payload.txt",
                zip::write::SimpleFileOptions::default().large_file(true),
            )
            .unwrap();
        zip64_writer.write_all(b"forced ZIP64 payload").unwrap();
        zip64_writer.finish().unwrap();
        let zip64_output = temp.path().join("forced-small");
        let zip64_guard =
            crate::operations::model::test_zip_create_guard(&zip64_archive_path, &zip64_output);
        let zip64_report = run(OperationRequest::extract_zip(
            zip64_archive_path.clone(),
            zip64_output.clone(),
            zip64_guard,
        )
        .unwrap());
        assert_eq!(
            zip64_report.outcome,
            TerminalOutcome::Succeeded,
            "{:?}",
            zip64_report.receipts[0].detail
        );
        assert_eq!(
            std::fs::read(zip64_output.join("nested/payload.txt")).unwrap(),
            b"forced ZIP64 payload"
        );
        assert!(zip64_output.join("empty").is_dir());
        assert!(zip64_archive_path.is_file());

        let empty_archive_path = temp.path().join("empty.zip");
        zip::ZipWriter::new(File::create(&empty_archive_path).unwrap())
            .finish()
            .unwrap();
        let empty_output_path = temp.path().join("empty");
        let empty_guard = crate::operations::model::test_zip_create_guard(
            &empty_archive_path,
            &empty_output_path,
        );
        let empty_request = OperationRequest::extract_zip(
            empty_archive_path.clone(),
            empty_output_path.clone(),
            empty_guard,
        )
        .unwrap();
        let empty_report = run(empty_request);
        assert_eq!(empty_report.outcome, TerminalOutcome::Succeeded);
        assert!(empty_output_path.is_dir());
        assert_eq!(std::fs::read_dir(&empty_output_path).unwrap().count(), 0);
        assert!(empty_archive_path.is_file());

        let oversized_archive_path = temp.path().join("oversized-metadata.zip");
        let mut oversized_writer =
            zip::ZipWriter::new(File::create(&oversized_archive_path).unwrap());
        oversized_writer
            .start_file(
                "large.bin",
                zip::write::SimpleFileOptions::default().large_file(true),
            )
            .unwrap();
        oversized_writer.write_all(b"x").unwrap();
        oversized_writer.finish().unwrap();
        let mut oversized_bytes = std::fs::read(&oversized_archive_path).unwrap();
        let central = oversized_bytes
            .windows(4)
            .rposition(|window| window == b"PK\x01\x02")
            .unwrap();
        let name_len =
            u16::from_le_bytes([oversized_bytes[central + 28], oversized_bytes[central + 29]])
                as usize;
        let extra = central + 46 + name_len;
        assert_eq!(&oversized_bytes[extra..extra + 2], &1u16.to_le_bytes());
        oversized_bytes[extra + 4..extra + 12]
            .copy_from_slice(&(MAX_ENTRY_BYTES + 1).to_le_bytes());
        std::fs::write(&oversized_archive_path, oversized_bytes).unwrap();
        let oversized_output = temp.path().join("oversized-metadata");
        let oversized_guard = crate::operations::model::test_zip_create_guard(
            &oversized_archive_path,
            &oversized_output,
        );
        let oversized_request = OperationRequest::extract_zip(
            oversized_archive_path.clone(),
            oversized_output.clone(),
            oversized_guard,
        )
        .unwrap();
        let token = crate::operations::model::OperationService::default()
            .submit(oversized_request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let mut planned_phases = Vec::new();
        let terminal = Mutex::new(None);
        execute(
            token,
            &oversized_request,
            &AtomicBool::new(false),
            &control_rx,
            &mut |fact| match fact {
                Fact::ZipExtractLedgerIntent {
                    token,
                    item,
                    ledger,
                } => {
                    planned_phases.push(ledger.phase);
                    control_tx
                        .send(WorkerControl::ZipExtractLedgerJournaledExact {
                            token,
                            item,
                            ledger,
                        })
                        .unwrap();
                }
                Fact::Terminal { report, .. } => *terminal.lock().unwrap() = Some(report),
                _ => {}
            },
        );
        assert!(
            planned_phases.is_empty(),
            "oversized metadata must fail before StagePlanned"
        );
        assert_eq!(
            terminal.into_inner().unwrap().unwrap().outcome,
            TerminalOutcome::Failed
        );
        assert!(!oversized_output.exists());
        assert_eq!(stage_count(), 0);

        let aggregate_archive_path = temp.path().join("aggregate-metadata.zip");
        let mut aggregate_writer =
            zip::ZipWriter::new(File::create(&aggregate_archive_path).unwrap());
        for name in ["one.bin", "two.bin", "three.bin"] {
            aggregate_writer
                .start_file(
                    name,
                    zip::write::SimpleFileOptions::default().large_file(true),
                )
                .unwrap();
            aggregate_writer.write_all(b"x").unwrap();
        }
        aggregate_writer.finish().unwrap();
        let mut aggregate_bytes = std::fs::read(&aggregate_archive_path).unwrap();
        let central_offsets = aggregate_bytes
            .windows(4)
            .enumerate()
            .filter_map(|(index, signature)| (signature == b"PK\x01\x02").then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(central_offsets.len(), 3);
        for central in central_offsets {
            let name_len =
                u16::from_le_bytes([aggregate_bytes[central + 28], aggregate_bytes[central + 29]])
                    as usize;
            let extra = central + 46 + name_len;
            assert_eq!(&aggregate_bytes[extra..extra + 2], &1u16.to_le_bytes());
            aggregate_bytes[extra + 4..extra + 12].copy_from_slice(&MAX_ENTRY_BYTES.to_le_bytes());
        }
        std::fs::write(&aggregate_archive_path, aggregate_bytes).unwrap();
        let aggregate_output = temp.path().join("aggregate-metadata");
        let aggregate_guard = crate::operations::model::test_zip_create_guard(
            &aggregate_archive_path,
            &aggregate_output,
        );
        let aggregate_request = OperationRequest::extract_zip(
            aggregate_archive_path.clone(),
            aggregate_output.clone(),
            aggregate_guard,
        )
        .unwrap();
        let token = crate::operations::model::OperationService::default()
            .submit(aggregate_request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let mut aggregate_phases = Vec::new();
        let terminal = Mutex::new(None);
        execute(
            token,
            &aggregate_request,
            &AtomicBool::new(false),
            &control_rx,
            &mut |fact| match fact {
                Fact::ZipExtractLedgerIntent {
                    token,
                    item,
                    ledger,
                } => {
                    aggregate_phases.push(ledger.phase);
                    control_tx
                        .send(WorkerControl::ZipExtractLedgerJournaledExact {
                            token,
                            item,
                            ledger,
                        })
                        .unwrap();
                }
                Fact::Terminal { report, .. } => *terminal.lock().unwrap() = Some(report),
                _ => {}
            },
        );
        assert!(
            aggregate_phases.is_empty(),
            "aggregate overflow must fail before StagePlanned"
        );
        assert_eq!(
            terminal.into_inner().unwrap().unwrap().outcome,
            TerminalOutcome::Failed
        );
        assert!(!aggregate_output.exists());
        assert_eq!(stage_count(), 0);
    }

    #[test]
    fn multi_extract_keeps_order_and_continues_after_collision_and_malformed_archive() {
        use std::sync::{Mutex, mpsc};
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let collision_zip = temp.path().join("collision.zip");
        let mut writer = zip::ZipWriter::new(File::create(&collision_zip).unwrap());
        writer
            .start_file("value.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"first").unwrap();
        writer.finish().unwrap();
        let malformed_zip = temp.path().join("bad.zip");
        std::fs::write(&malformed_zip, b"not a zip archive").unwrap();
        let good_zip = temp.path().join("good.zip");
        let mut writer = zip::ZipWriter::new(File::create(&good_zip).unwrap());
        writer
            .start_file("value.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"last succeeds").unwrap();
        writer.finish().unwrap();
        let collision_output = temp.path().join("collision");
        std::fs::create_dir(&collision_output).unwrap();
        std::fs::write(collision_output.join("sentinel"), b"keep").unwrap();
        let sources = vec![
            collision_zip.clone(),
            malformed_zip.clone(),
            good_zip.clone(),
        ];
        let guards = sources
            .iter()
            .map(|source| {
                let output = source.parent().unwrap().join(source.file_stem().unwrap());
                let guard = crate::operations::model::test_zip_create_guard(source, &output);
                crate::operations::model::ZipCreateSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }
            })
            .collect();
        let parent_guard =
            crate::operations::model::test_zip_create_guard(&good_zip, &temp.path().join("unused"));
        let request = OperationRequest::extract_zip_many(
            sources,
            crate::operations::model::ZipArchiveExtractGuard {
                destination_parent_identity: parent_guard.destination_parent_identity,
                sources: guards,
            },
        )
        .unwrap();
        let token = crate::operations::model::OperationService::default()
            .submit(request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let terminal = Mutex::new(Vec::new());
        execute(
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
                Fact::Terminal { report, .. } => terminal.lock().unwrap().push(report),
                _ => {}
            },
        );
        let reports = terminal.into_inner().unwrap();
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.outcome, TerminalOutcome::Partial);
        assert_eq!(
            report
                .receipts
                .iter()
                .map(|receipt| receipt.item)
                .collect::<Vec<_>>(),
            vec![ItemId(0), ItemId(1), ItemId(2)]
        );
        assert_eq!(
            report
                .receipts
                .iter()
                .map(|receipt| receipt.outcome)
                .collect::<Vec<_>>(),
            vec![
                ItemOutcome::Failed,
                ItemOutcome::Failed,
                ItemOutcome::Completed
            ]
        );
        assert_eq!(report.receipts[0].created_folder_path, None);
        assert_eq!(
            report.receipts[2].created_folder_path.as_deref(),
            Some(temp.path().join("good").as_path())
        );
        assert_eq!(
            std::fs::read(collision_output.join("sentinel")).unwrap(),
            b"keep"
        );
        assert_eq!(
            std::fs::read(temp.path().join("good/value.txt")).unwrap(),
            b"last succeeds"
        );
        assert_eq!(report.final_counters.processed_items, 1);
        assert_eq!(report.final_counters.total_bytes, None);
    }

    #[test]
    fn multi_extract_cancellation_between_items_keeps_first_output_and_one_terminal() {
        use std::sync::{Mutex, mpsc};
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let mut sources = Vec::new();
        let mut guards = Vec::new();
        for name in ["one", "two"] {
            let source = temp.path().join(format!("{name}.zip"));
            let mut writer = zip::ZipWriter::new(File::create(&source).unwrap());
            writer
                .start_file("value.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(name.as_bytes()).unwrap();
            writer.finish().unwrap();
            let output = temp.path().join(name);
            let guard = crate::operations::model::test_zip_create_guard(&source, &output);
            guards.push(crate::operations::model::ZipCreateSourceGuard {
                source_identity: guard.source_identity,
                source_kind: guard.source_kind,
                entry_name: guard.entry_name,
            });
            sources.push(source);
        }
        let parent_guard = crate::operations::model::test_zip_create_guard(
            &sources[0],
            &temp.path().join("unused"),
        );
        let request = OperationRequest::extract_zip_many(
            sources.clone(),
            crate::operations::model::ZipArchiveExtractGuard {
                destination_parent_identity: parent_guard.destination_parent_identity,
                sources: guards,
            },
        )
        .unwrap();
        let token = crate::operations::model::OperationService::default()
            .submit(request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let cancelled = AtomicBool::new(false);
        let terminal = Mutex::new(Vec::new());
        execute(
            token,
            &request,
            &cancelled,
            &control_rx,
            &mut |fact| match fact {
                Fact::ZipExtractLedgerIntent {
                    token,
                    item,
                    ledger,
                } => {
                    let published = ledger.phase == Phase::Published && item == ItemId(0);
                    control_tx
                        .send(WorkerControl::ZipExtractLedgerJournaledExact {
                            token,
                            item,
                            ledger,
                        })
                        .unwrap();
                    if published {
                        cancelled.store(true, AtomicOrdering::Release);
                    }
                }
                Fact::Terminal { report, .. } => terminal.lock().unwrap().push(report),
                _ => {}
            },
        );
        let reports = terminal.into_inner().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, TerminalOutcome::Cancelled);
        assert_eq!(
            reports[0]
                .receipts
                .iter()
                .map(|r| r.item)
                .collect::<Vec<_>>(),
            vec![ItemId(0), ItemId(1)]
        );
        assert_eq!(reports[0].receipts[0].outcome, ItemOutcome::Completed);
        assert_eq!(
            reports[0].receipts[1].outcome,
            ItemOutcome::CancelledBeforeStart
        );
        assert!(temp.path().join("one/value.txt").is_file());
        assert!(!temp.path().join("two").exists());
    }

    #[test]
    fn later_source_replacement_while_first_item_runs_fails_closed() {
        use std::sync::{Mutex, mpsc};
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let sources = [temp.path().join("first.zip"), temp.path().join("later.zip")];
        for source in &sources {
            let mut writer = zip::ZipWriter::new(File::create(source).unwrap());
            writer
                .start_file("payload", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"stable source").unwrap();
            writer.finish().unwrap();
        }
        let guards = sources
            .iter()
            .map(|source| {
                let guard = crate::operations::model::test_zip_create_guard(
                    source,
                    &source.with_extension(""),
                );
                crate::operations::model::ZipCreateSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }
            })
            .collect();
        let parent = crate::operations::model::test_zip_create_guard(
            &sources[0],
            &temp.path().join("unused"),
        );
        let request = OperationRequest::extract_zip_many(
            sources.to_vec(),
            crate::operations::model::ZipArchiveExtractGuard {
                destination_parent_identity: parent.destination_parent_identity,
                sources: guards,
            },
        )
        .unwrap();
        let token = crate::operations::model::OperationService::default()
            .submit(request.clone())
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel();
        let terminal = Mutex::new(Vec::new());
        let replaced = AtomicBool::new(false);
        execute(
            token,
            &request,
            &AtomicBool::new(false),
            &control_rx,
            &mut |fact| match fact {
                Fact::ZipExtractLedgerIntent {
                    token,
                    item,
                    ledger,
                } => {
                    if item == ItemId(0)
                        && ledger.phase == Phase::StageOwned
                        && !replaced.swap(true, AtomicOrdering::AcqRel)
                    {
                        let moved = sources[1].with_extension("old");
                        std::fs::rename(&sources[1], moved).unwrap();
                        std::fs::write(&sources[1], b"replacement bytes").unwrap();
                    }
                    control_tx
                        .send(WorkerControl::ZipExtractLedgerJournaledExact {
                            token,
                            item,
                            ledger,
                        })
                        .unwrap();
                }
                Fact::Terminal { report, .. } => terminal.lock().unwrap().push(report),
                _ => {}
            },
        );
        let reports = terminal.into_inner().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, TerminalOutcome::Partial);
        assert_eq!(reports[0].receipts[0].outcome, ItemOutcome::Completed);
        assert_eq!(reports[0].receipts[1].outcome, ItemOutcome::Failed);
        assert!(temp.path().join("first/payload").is_file());
        assert!(!temp.path().join("later").exists());
    }

    #[test]
    fn queued_multi_cancel_reports_every_item_before_start() {
        use std::sync::{Mutex, mpsc};
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let sources = [temp.path().join("a.zip"), temp.path().join("b.zip")];
        let guards = sources
            .iter()
            .map(|source| {
                std::fs::write(source, b"not inspected after queued cancel").unwrap();
                let guard = crate::operations::model::test_zip_create_guard(
                    source,
                    &source.with_extension(""),
                );
                crate::operations::model::ZipCreateSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }
            })
            .collect();
        let parent = crate::operations::model::test_zip_create_guard(
            &sources[0],
            &temp.path().join("unused"),
        );
        let request = OperationRequest::extract_zip_many(
            sources.to_vec(),
            crate::operations::model::ZipArchiveExtractGuard {
                destination_parent_identity: parent.destination_parent_identity,
                sources: guards,
            },
        )
        .unwrap();
        let token = crate::operations::model::OperationService::default()
            .submit(request.clone())
            .unwrap();
        let (_control_tx, control_rx) = mpsc::channel();
        let cancelled = AtomicBool::new(true);
        let terminal = Mutex::new(Vec::new());
        execute(token, &request, &cancelled, &control_rx, &mut |fact| {
            if let Fact::Terminal { report, .. } = fact {
                terminal.lock().unwrap().push(report);
            }
        });
        let reports = terminal.into_inner().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, TerminalOutcome::Cancelled);
        assert_eq!(
            reports[0]
                .receipts
                .iter()
                .map(|r| r.outcome)
                .collect::<Vec<_>>(),
            vec![ItemOutcome::CancelledBeforeStart; 2]
        );
        assert_eq!(reports[0].final_counters.total_items, Some(2));
    }
}
