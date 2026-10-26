//! Shell-owned execution lane for file operations.
//!
//! `OperationService` is still the lifecycle authority.  This small adapter
//! only gives an accepted attempt one cancellation flag and ensures that a
//! terminal fact is committed before its flag is forgotten or another queued
//! attempt is started.  GPUI and filesystem work deliberately stay outside so
//! the same ordering can be tested without a window.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{
    executor::{ExecutorFact, WorkerControl},
    model::{
        AttemptToken, ConflictDecision, ConflictToken, ExecutorCommand, JobSnapshot,
        OperationRequest, OperationService, ProgressUpdate, RetryError, ServiceEvent,
        ServiceEventOrCommand, SubmitError, TerminalOutcome,
    },
};

/// Shared only with the detached executor for this exact attempt.
pub type Cancellation = Arc<AtomicBool>;
const UNDO_TRASH_LIMIT: usize = 100;

fn unknown_operation_change_hints(request: &OperationRequest) -> Vec<super::model::FsChangeHint> {
    let mut directories = Vec::new();
    let mut push_directory = |directory: PathBuf| {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    };

    for source in &request.sources {
        if let Some(parent) = source.parent() {
            push_directory(parent.to_path_buf());
        }
    }

    match request.kind {
        super::model::OperationKind::Copy
        | super::model::OperationKind::Move
        | super::model::OperationKind::CreateSymbolicLink
        | super::model::OperationKind::CreateJunction
        | super::model::OperationKind::CreateFolder
        | super::model::OperationKind::CreateFile
        | super::model::OperationKind::CreateShortcut => {
            if let Some(destination) = &request.destination {
                push_directory(destination.clone());
            }
        }
        super::model::OperationKind::CreateZip | super::model::OperationKind::Create7z => {
            if let Some(destination) = &request.destination {
                if let Some(parent) = destination.parent() {
                    push_directory(parent.to_path_buf());
                }
            }
        }
        super::model::OperationKind::ExtractZip => {
            if let Some(destination) = &request.destination {
                if let Some(parent) = destination.parent() {
                    push_directory(parent.to_path_buf());
                }
            }
        }
        super::model::OperationKind::Extract7z => {
            if let Some(destination) = &request.destination {
                if let Some(parent) = destination.parent() {
                    push_directory(parent.to_path_buf());
                }
            }
        }
        super::model::OperationKind::Rename => {
            if let Some(parent) = request
                .destination
                .as_deref()
                .and_then(std::path::Path::parent)
            {
                push_directory(parent.to_path_buf());
            }
        }
        super::model::OperationKind::CopyStageCleanup => {}
        super::model::OperationKind::Trash
        | super::model::OperationKind::BatchRename
        | super::model::OperationKind::BatchRenameUndo
        | super::model::OperationKind::ExternalMoveCleanup => {}
    }

    if let Some(plan) = &request.batch_rename_plan {
        for target in plan {
            if let Some(parent) = target.destination.parent() {
                push_directory(parent.to_path_buf());
            }
        }
    }

    directories
        .into_iter()
        .map(|directory| super::model::FsChangeHint { directory })
        .collect()
}

#[derive(Clone, Debug)]
pub enum UndoEntry {
    Trash(super::trash::TrashReceipt),
    TrashGroup(Vec<super::trash::TrashReceipt>),
    Copy(super::model::CopyUndoReceipt),
    DirectoryCopy(super::model::DirectoryCopyUndoReceipt),
    DirectoryCopyGroup(Vec<super::model::DirectoryCopyUndoReceipt>),
    MixedCopyGroup(Vec<super::model::CopyUndoItem>),
    CreatedObject(super::model::CreatedObjectUndoReceipt),
    Move(Vec<super::model::MoveUndoReceipt>),
    BatchRename {
        receipt_id: BatchRenameUndoId,
        targets: Vec<super::batch_rename::RenameTarget>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BatchRenameUndoId(super::model::JobId);

#[derive(Clone, Debug)]
struct BatchUndoAssociation {
    receipt_id: BatchRenameUndoId,
    targets: Vec<super::batch_rename::RenameTarget>,
}

#[derive(Clone, Debug)]
struct PendingBatchUndo {
    association: BatchUndoAssociation,
    must_still_be_current: bool,
}

fn retained_batch_undo_targets(
    original: &[super::batch_rename::RenameTarget],
    reverse_plan: &[super::batch_rename::RenameTarget],
    receipts: &[super::model::ItemReceipt],
) -> Vec<super::batch_rename::RenameTarget> {
    let mut resolved_items: Vec<_> = receipts
        .iter()
        .take_while(|receipt| receipt.outcome == super::model::ItemOutcome::Completed)
        .map(|receipt| receipt.item)
        .collect();
    if let Some(receipt) = receipts.get(resolved_items.len())
        && matches!(
            receipt.outcome,
            super::model::ItemOutcome::Partial | super::model::ItemOutcome::Unknown
        )
    {
        resolved_items.push(receipt.item);
    }

    original
        .iter()
        .filter(|target| {
            reverse_plan
                .iter()
                .enumerate()
                .find(|(_, reverse)| {
                    reverse.source == target.destination && reverse.destination == target.source
                })
                .is_none_or(|(index, _)| {
                    !resolved_items.contains(&super::model::ItemId(index as u32))
                })
        })
        .cloned()
        .collect()
}

/// Ephemeral, one-shot keepalive for source leaves authorized by Windows OLE.
/// It is deliberately separate from the cloneable/history request value.
#[derive(Debug)]
pub struct SourceCleanupLease {
    pub paths: Vec<PathBuf>,
    pub handles: Vec<File>,
    pub parent_routes: Vec<Vec<super::model::SourceCleanupParentIdentity>>,
    pub leaf_signatures: Vec<super::model::SourceCleanupLeafSignature>,
}

/// Process-local capability for one ordinary Copy stage. It is deliberately
/// non-cloneable and is never represented in the journal or request history.
#[derive(Debug)]
pub struct CopyStageCustody {
    pub token: AttemptToken,
    pub item: super::model::ItemId,
    pub path: PathBuf,
    pub identity: crate::filesystem_identity::FilesystemIdentity,
    pub parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub handle: File,
}

/// Cloneable channel envelope around one move-only capability. Cloned fact
/// envelopes share a single take slot, so only one receiver can obtain the
/// underlying handle and all abandoned copies merely close it on drop.
#[derive(Clone, Debug)]
pub struct CopyStageCustodyTransfer(std::sync::Arc<std::sync::Mutex<Option<CopyStageCustody>>>);

impl CopyStageCustodyTransfer {
    pub fn new(custody: CopyStageCustody) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Some(custody))))
    }

    pub fn take(&self) -> Option<CopyStageCustody> {
        self.0.lock().ok()?.take()
    }
}

/// A one-shot cleanup capability moved to the executor only after admission.
#[derive(Debug)]
pub struct CopyStageCleanupLease {
    pub token: AttemptToken,
    pub item: super::model::ItemId,
    pub path: PathBuf,
    pub identity: crate::filesystem_identity::FilesystemIdentity,
    pub parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub handle: File,
    #[cfg(test)]
    pub native_disposition_observer: Option<async_channel::Sender<Result<(), u32>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SevenZipExtractCleanupTarget {
    Spool,
    Tree,
}

fn seven_zip_extract_cleanup_transition(
    previous: Option<&super::journal::JournalSevenZipExtractBatch>,
    next: &super::journal::JournalSevenZipExtractBatch,
) -> Option<(super::model::ItemId, SevenZipExtractCleanupTarget)> {
    use super::journal::JournalSevenZipExtractObjectState as State;
    let previous = previous?;
    if next.items.len() != previous.items.len() || next.envelope != previous.envelope {
        return None;
    }
    let mut found = None;
    for (old, new) in previous.items.iter().zip(&next.items) {
        if old == new {
            continue;
        }
        if found.is_some() || !new.follows(old) {
            return None;
        }
        let target = match (
            old.spool_state,
            new.spool_state,
            old.tree_state,
            new.tree_state,
        ) {
            (State::Owned, State::DeleteIntent, a, b) if a == b => {
                SevenZipExtractCleanupTarget::Spool
            }
            (a, b, State::Owned, State::DeleteIntent) if a == b => {
                SevenZipExtractCleanupTarget::Tree
            }
            _ => return None,
        };
        found = Some((super::model::ItemId(new.binding.item), target));
    }
    found
}

#[derive(Clone, Debug, Default)]
struct SevenZipExtractRuntimeState {
    durable: Option<super::journal::JournalSevenZipExtractBatch>,
    pending: Option<super::journal::JournalSevenZipExtractBatch>,
    pending_record: Option<super::journal::JournalAttempt>,
    pending_write_claimed: bool,
    failed: Option<super::journal::JournalSevenZipExtractBatch>,
    failed_record: Option<super::journal::JournalAttempt>,
    frozen_terminal_record: Option<super::journal::JournalAttempt>,
    journal_failed: bool,
    cancel_requested: bool,
    terminal_journal_acknowledged: bool,
    terminal_delivery_closed: bool,
    #[cfg(test)]
    reject_terminal_ack_once: bool,
    #[cfg(test)]
    terminal_ack_rejection_consumed: bool,
}

fn promote_extract_retry_record(record: Option<&mut super::journal::JournalAttempt>) {
    if let Some(record) = record
        && !matches!(
            record.state,
            super::journal::JournalState::Terminal | super::journal::JournalState::RecoveryRequired
        )
    {
        record.state = super::journal::JournalState::CancelRequested;
    }
}

fn directory_move_replace_rename_immutable_fields_match(
    old: &super::journal::JournalDirectoryMoveReplaceRename,
    new: &super::journal::JournalDirectoryMoveReplaceRename,
) -> bool {
    old.item == new.item
        && old.source == new.source
        && old.destination == new.destination
        && old.backup == new.backup
        && old.source_identity == new.source_identity
        && old.approved_target_identity == new.approved_target_identity
        && old.volume_identity == new.volume_identity
        && old.source_parent == new.source_parent
        && old.source_parent_identity == new.source_parent_identity
        && old.destination_parent == new.destination_parent
        && old.destination_parent_identity == new.destination_parent_identity
        && old.approved_target_children.len() == new.approved_target_children.len()
        && old.approved_target_children.iter().zip(&new.approved_target_children).all(|(old, new)| {
            old.ordinal == new.ordinal && old.node_kind == new.node_kind
                && old.relative_components == new.relative_components && old.identity == new.identity
                && old.content_fact == new.content_fact
        })
}

fn directory_move_replace_rename_progress_matches(
    old: &super::journal::JournalDirectoryMoveReplaceRename,
    new: &super::journal::JournalDirectoryMoveReplaceRename,
) -> bool {
    old.approved_target_children.iter().map(|child| child.disposition_recorded)
        .eq(new.approved_target_children.iter().map(|child| child.disposition_recorded))
        && old.pending_cleanup == new.pending_cleanup
        && old.backup_root_disposed == new.backup_root_disposed
}

fn directory_move_replace_rename_matches_candidate(
    candidate: &super::model::DirectoryMoveReplaceRenameCandidate,
    ledger: &super::journal::JournalDirectoryMoveReplaceRename,
) -> bool {
    use super::journal::{JournalDirectoryNodeKind as Kind, JournalPath};
    let identity_text = |identity: &crate::filesystem_identity::FilesystemIdentity| {
        identity.windows_parts().map(|(volume, index)| format!("{volume}:{index:016x}"))
    };
    let backup = ledger.backup.to_path();
    let expected_children = candidate.approved_target_children.iter().map(|child| {
        (child.ordinal, match child.node_kind {
            super::model::DirectoryMoveReplaceRenameCandidateNodeKind::File => Kind::File,
            super::model::DirectoryMoveReplaceRenameCandidateNodeKind::Directory => Kind::Directory,
        }, child.relative_components.clone(), child.identity.clone(), child.content_fact.clone())
    }).collect::<Vec<_>>();
    let actual_children = ledger.approved_target_children.iter().map(|child| {
        (child.ordinal, child.node_kind, child.relative_components.clone(), child.identity.clone(), child.content_fact.clone())
    }).collect::<Vec<_>>();
    candidate.source_is_ordinary_directory
        && candidate.target_is_ordinary_directory
        && candidate.source == ledger.source.to_path().unwrap_or_default()
        && candidate.destination == ledger.destination.to_path().unwrap_or_default()
        && candidate.source_identity.windows_parts().is_some_and(|(volume, _)| volume == candidate.volume_identity)
        && identity_text(&candidate.source_identity).as_deref() == Some(ledger.source_identity.as_str())
        && identity_text(&candidate.approved_target_identity).as_deref() == Some(ledger.approved_target_identity.as_str())
        && ledger.volume_identity == format!("{:08x}", candidate.volume_identity)
        && candidate.source_parent == ledger.source_parent.to_path().unwrap_or_default()
        && identity_text(&candidate.source_parent_identity).as_deref() == Some(ledger.source_parent_identity.as_str())
        && candidate.destination_parent == ledger.destination_parent.to_path().unwrap_or_default()
        && identity_text(&candidate.destination_parent_identity).as_deref() == Some(ledger.destination_parent_identity.as_str())
        && ledger.item == 0
        && backup.as_ref().and_then(|path| path.parent()) == Some(candidate.destination_parent.as_path())
        && expected_children == actual_children
        && matches!(ledger.phase,
            super::journal::DirectoryMoveReplaceRenamePhase::Prepared
                | super::journal::DirectoryMoveReplaceRenamePhase::NoEffect
                | super::journal::DirectoryMoveReplaceRenamePhase::DisplaceIntent
                | super::journal::DirectoryMoveReplaceRenamePhase::TargetDisplaced
                | super::journal::DirectoryMoveReplaceRenamePhase::PublishIntent
                | super::journal::DirectoryMoveReplaceRenamePhase::DestinationPublished
                | super::journal::DirectoryMoveReplaceRenamePhase::BackupCleanup
                | super::journal::DirectoryMoveReplaceRenamePhase::BackupComplete
                | super::journal::DirectoryMoveReplaceRenamePhase::Complete)
        && (ledger.phase != super::journal::DirectoryMoveReplaceRenamePhase::Prepared
            || (ledger.source_location == super::journal::DirectoryMoveReplaceSourceLocation::Original
                && ledger.target_location == super::journal::DirectoryMoveReplaceTargetLocation::Destination))
        && candidate.source != candidate.destination
        && JournalPath::from_path(&candidate.source) == ledger.source
}

impl SourceCleanupLease {
    pub fn from_parts(
        paths: Vec<PathBuf>,
        handles: Vec<File>,
        parent_routes: Vec<Vec<super::model::SourceCleanupParentIdentity>>,
        leaf_signatures: Vec<super::model::SourceCleanupLeafSignature>,
    ) -> Self {
        Self {
            paths,
            handles,
            parent_routes,
            leaf_signatures,
        }
    }
}

fn valid_cleanup_parent_routes(
    sources: &[PathBuf],
    routes: &[Vec<super::model::SourceCleanupParentIdentity>],
) -> bool {
    sources.len() == routes.len()
        && sources.iter().zip(routes).all(|(source, route)| {
            let Some(parent) = source.parent() else {
                return false;
            };
            let expected = parent.ancestors().map(PathBuf::from).collect::<Vec<_>>();
            let expected = expected.into_iter().rev().collect::<Vec<_>>();
            !route.is_empty()
                && route.len() == expected.len()
                && route.iter().zip(expected).all(|(captured, expected)| {
                    captured.path == expected && captured.identity.windows_parts().is_some()
                })
        })
}

/// Immutable worker input.  A caller starts it on a background executor and
/// returns each fact through [`OperationRuntime::commit_fact`].
#[derive(Debug)]
pub struct WorkerStart {
    pub command: ExecutorCommand,
    pub cancellation: Cancellation,
    pub controls: Receiver<WorkerControl>,
    pub source_cleanup_lease: Option<SourceCleanupLease>,
    pub copy_stage_cleanup_lease: Option<CopyStageCleanupLease>,
    /// Cleanup deletion waits for the exact Running journal acknowledgment.
    pub copy_stage_cleanup_running_ack: Option<async_channel::Receiver<()>>,
    /// 7z extraction worker identity bound to this runtime run and attempt.
    pub seven_zip_extract_attempt: Option<super::journal::JournalSevenZipExtractAttemptId>,
}

/// A terminal commit is the only point at which a caller may reconcile
/// directory views or start the next queued operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCommit {
    pub event: ServiceEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrdinaryCopyRequestClassification {
    AllFiles,
    AllDirectories,
    Mixed,
    MixedIncomplete,
    Indeterminate,
}

fn windows_ordinal_leaf_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let left = left.encode_wide().collect::<Vec<_>>();
        let right = right.encode_wide().collect::<Vec<_>>();
        unsafe {
            windows::Win32::Globalization::CompareStringOrdinal(
                &left,
                &right,
                windows::Win32::Foundation::BOOL(1),
            )
        }
        .0 == 2
    }
    #[cfg(not(windows))]
    {
        left.to_string_lossy()
            .chars()
            .flat_map(char::to_lowercase)
            .eq(right.to_string_lossy().chars().flat_map(char::to_lowercase))
    }
}

/// One serial filesystem lane above the lifecycle model.
///
/// The lane is intentionally conservative for the first Windows adapters:
/// overlapping mutations make destination validation and directory hints much
/// harder to reason about.  The model itself remains capable of queueing jobs.
pub struct OperationRuntime {
    service: OperationService,
    run_id: Option<[u8; 16]>,
    running: Option<AttemptToken>,
    cancellations: HashMap<AttemptToken, Cancellation>,
    controls: HashMap<AttemptToken, Sender<WorkerControl>>,
    /// Receipts are accepted only for the live attempt and become undoable
    /// only after that attempt's terminal report was accepted.
    pending_trash: HashMap<AttemptToken, Vec<(super::model::ItemId, super::trash::TrashReceipt)>>,
    replace_backups: HashMap<AttemptToken, super::journal::JournalReplaceBackup>,
    pending_replace_intents: HashMap<
        AttemptToken,
        (
            super::model::ItemId,
            std::path::PathBuf,
            std::path::PathBuf,
            std::path::PathBuf,
        ),
    >,
    cross_volume_move_replaces:
        HashMap<AttemptToken, super::journal::JournalCrossVolumeMoveReplace>,
    pending_cross_volume_move_replace:
        HashMap<AttemptToken, super::journal::JournalCrossVolumeMoveReplace>,
    directory_move_replace_rename:
        HashMap<AttemptToken, super::journal::JournalDirectoryMoveReplaceRename>,
    pending_directory_move_replace_rename:
        HashMap<AttemptToken, (super::model::ConflictToken, super::journal::JournalDirectoryMoveReplaceRename)>,
    pending_directory_move_replace_rename_cleanup: HashMap<AttemptToken, (super::journal::JournalDirectoryMoveReplaceRenameChildIntent, RenameCleanupBarrier)>,
    rename_stages: HashMap<AttemptToken, super::journal::JournalRenameStage>,
    zip_creates: HashMap<AttemptToken, super::journal::JournalZipCreate>,
    pending_zip_create_intents: HashMap<AttemptToken, super::journal::JournalZipCreate>,
    seven_zip_creates: HashMap<AttemptToken, super::journal::JournalSevenZipHandleCreate>,
    pending_seven_zip_create_intents:
        HashMap<AttemptToken, super::journal::JournalSevenZipHandleCreate>,
    failed_seven_zip_create_journal:
        HashMap<AttemptToken, super::journal::JournalSevenZipHandleCreate>,
    seven_zip_create_journal_failed: HashSet<AttemptToken>,
    zip_extracts: HashMap<(AttemptToken, super::model::ItemId), super::journal::JournalZipExtract>,
    pending_zip_extract_intents:
        HashMap<(AttemptToken, super::model::ItemId), super::journal::JournalZipExtract>,
    seven_zip_extract_batches: HashMap<AttemptToken, SevenZipExtractRuntimeState>,
    pending_rename_intents: HashMap<
        AttemptToken,
        (
            super::model::ItemId,
            std::path::PathBuf,
            std::path::PathBuf,
            std::path::PathBuf,
            String,
            String,
            u8,
        ),
    >,
    batch_rename_completed: HashMap<AttemptToken, Vec<super::journal::JournalBatchRenameReceipt>>,
    pending_batch_rename_items:
        HashMap<AttemptToken, (super::model::ItemId, std::path::PathBuf, std::path::PathBuf)>,
    pending_batch_undo: HashMap<AttemptToken, PendingBatchUndo>,
    batch_undo_receipts: HashMap<super::model::JobId, BatchUndoAssociation>,
    pending_batch_plan_intents: HashMap<AttemptToken, Vec<super::batch_rename::RenameTarget>>,
    /// Transient exact barrier only; JournalStore remains the durable ledger owner.
    directory_moves: HashMap<(AttemptToken, super::model::ItemId), DirectoryMoveRuntimeState>,
    directory_move_replaces:
        HashMap<(AttemptToken, super::model::ItemId), DirectoryMoveReplaceRuntimeState>,
    copy_replace_cleanups:
        HashMap<(AttemptToken, super::model::ItemId), CopyReplaceCleanupRuntimeState>,
    keep_both_directory_items: HashSet<(AttemptToken, super::model::ItemId)>,
    source_cleanup_leases: HashMap<AttemptToken, SourceCleanupLease>,
    copy_stage_custody: HashMap<(AttemptToken, super::model::ItemId), CopyStageCustody>,
    copy_stage_cleanup_leases: HashMap<AttemptToken, CopyStageCleanupLease>,
    copy_stage_cleanup_running_acks: HashMap<
        AttemptToken,
        (super::journal::JournalAttempt, async_channel::Sender<()>),
    >,
    #[cfg(test)]
    copy_stage_cleanup_native_observers:
        HashMap<AttemptToken, async_channel::Sender<Result<(), u32>>>,
    undo_ledger: Vec<UndoEntry>,
    undo_reservation: Option<usize>,
    created_object_undo_candidates: HashMap<(AttemptToken, super::model::ItemId), super::model::CreatedObjectUndoReceipt>,
    invalid_created_object_undo_candidates: HashSet<(AttemptToken, super::model::ItemId)>,
    directory_copy_undo_candidates: HashMap<(AttemptToken, super::model::ItemId), super::model::DirectoryCopyUndoReceipt>,
    invalid_directory_copy_undo_candidates: HashSet<(AttemptToken, super::model::ItemId)>,
    ordinary_copy_source_kinds: HashMap<(AttemptToken, super::model::ItemId), super::executor::OrdinaryCopySourceKind>,
    // A job's source-kind decision belongs to its original request and spans retries.
    ordinary_copy_source_kinds_by_job: HashMap<super::model::JobId, OrdinaryCopyRequestClassification>,
    ordinary_copy_source_kinds_by_job_items: HashMap<super::model::JobId, Vec<super::executor::OrdinaryCopySourceKind>>,
    move_undo_candidates: HashMap<(AttemptToken, super::model::ItemId), super::model::MoveUndoReceipt>,
    invalid_move_undo_candidates: HashSet<(AttemptToken, super::model::ItemId)>,
    next_inspection_ticket: u64,
    pending_move_inspection: Option<super::model::GuardedMoveInspectionResult>,
    inspection_history:
        HashMap<super::model::InspectionTicket, super::model::GuardedMoveInspectionResult>,
    latest_move_inspection: HashMap<AttemptToken, super::model::InspectionTicket>,
    pending_cross_volume_move_inspection: Option<super::model::CrossVolumeMoveInspectionResult>,
    cross_volume_move_inspection_history: HashMap<super::model::InspectionTicket, super::model::CrossVolumeMoveInspectionResult>,
    latest_cross_volume_move_inspection: HashMap<AttemptToken, super::model::InspectionTicket>,
    pending_published_move_inspection: Option<super::model::PublishedMoveInspectionResult>,
    published_move_inspection_history: HashMap<super::model::InspectionTicket, super::model::PublishedMoveInspectionResult>,
    latest_published_move_inspection: HashMap<AttemptToken, super::model::InspectionTicket>,
    pending_copy_inspection: Option<super::model::CopyFailureInspectionResult>,
    copy_inspection_history:
        HashMap<super::model::InspectionTicket, super::model::CopyFailureInspectionResult>,
    latest_copy_inspection: HashMap<AttemptToken, super::model::InspectionTicket>,
    returned_attempts: HashSet<AttemptToken>,
    missing_terminal_candidates: HashSet<AttemptToken>,
    synthesized_unknown_attempts: HashSet<AttemptToken>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RenameCleanupBarrier { Ready, IntentPending, IntentDurable, ReceiptPending, BackupCompletePending }

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryMoveRuntimeState {
    item: super::model::ItemId,
    source: std::path::PathBuf,
    destination: std::path::PathBuf,
    barrier: DirectoryMoveBarrier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryMoveReplaceRuntimeState {
    ledger: super::journal::JournalDirectoryMoveReplace,
    phase: super::journal::DirectoryMoveReplacePhase,
    barrier: DirectoryMoveReplaceBarrier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectoryMoveReplaceBarrier {
    Ready,
    PhasePending(super::journal::DirectoryMoveReplacePhase),
    BackupIntentPending(super::journal::JournalDirectoryMoveReplaceChildIntent),
    BackupIntentDurable(super::journal::JournalDirectoryMoveReplaceChildIntent),
    BackupReceiptPending(super::journal::JournalDirectoryMoveReplaceChildIntent),
    BackupCompletePending,
    BackupComplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CopyReplaceCleanupRuntimeState {
    ledger: super::journal::JournalCopyReplaceCleanup,
    barrier: CopyReplaceCleanupBarrier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CopyReplaceCleanupBarrier {
    ManifestPending,
    Ready,
    IntentPending(super::journal::JournalCopyReplaceChildIntent),
    IntentDurable(super::journal::JournalCopyReplaceChildIntent),
    ReceiptPending(super::journal::JournalCopyReplaceChildIntent),
    CompletePending,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectoryMoveBarrier {
    ManifestPending(super::journal::JournalDirectoryMove),
    Ready,
    ChildIntentPending(super::journal::JournalDirectoryChildIntent),
    ChildIntentDurable(super::journal::JournalDirectoryChildIntent),
    ChildReceiptPending(super::journal::JournalDirectoryChildIntent),
    RootIntentPending(super::journal::JournalDirectoryRootIntent),
    RootIntentDurable(super::journal::JournalDirectoryRootIntent),
    RootReceiptPending(super::journal::JournalDirectoryRootIntent),
    SourceComplete,
    CompletePending,
    Complete,
    TerminalPending,
}

fn directory_move_replace_backup_next_intent(
    ledger: &super::journal::JournalDirectoryMoveReplace,
    intent: &super::journal::JournalDirectoryMoveReplaceChildIntent,
) -> bool {
    use super::journal::JournalDirectoryNodeKind;
    if intent.ordinal as usize == ledger.backup_children.len() {
        return intent.node_kind == JournalDirectoryNodeKind::Directory
            && intent.relative_components.is_empty()
            && intent.identity == ledger.backup_root_identity
            && intent.content_fact == "root"
            && ledger
                .backup_children
                .iter()
                .all(|child| child.disposition_recorded);
    }
    ledger
        .backup_children
        .get(intent.ordinal as usize)
        .is_some_and(|child| {
            !child.disposition_recorded
                && child.node_kind == intent.node_kind
                && child.relative_components == intent.relative_components
                && child.identity == intent.identity
                && child.content_fact == intent.content_fact
        })
}

impl Default for OperationRuntime {
    fn default() -> Self {
        Self::new(OperationService::default())
    }
}

impl OperationRuntime {
    pub fn new(service: OperationService) -> Self {
        Self::with_journal_keys(service, std::iter::empty::<String>())
    }

    pub fn with_journal_keys(
        service: OperationService,
        journal_keys: impl IntoIterator<Item = String>,
    ) -> Self {
        let journal_keys: Vec<String> = journal_keys.into_iter().collect();
        let run_id = (|| -> Result<[u8; 16], String> {
            loop {
                let mut candidate = [0u8; 16];
                getrandom::fill(&mut candidate).map_err(|error| error.to_string())?;
                let prefix = format!("r:{}:", super::journal::hex_run_id(&candidate));
                if !journal_keys.iter().any(|key| key.starts_with(&prefix)) {
                    return Ok(candidate);
                }
            }
        })()
        .ok();
        Self {
            service,
            run_id,
            running: None,
            cancellations: HashMap::new(),
            controls: HashMap::new(),
            pending_trash: HashMap::new(),
            replace_backups: HashMap::new(),
            pending_replace_intents: HashMap::new(),
            cross_volume_move_replaces: HashMap::new(),
            pending_cross_volume_move_replace: HashMap::new(),
            directory_move_replace_rename: HashMap::new(),
            pending_directory_move_replace_rename: HashMap::new(),
            pending_directory_move_replace_rename_cleanup: HashMap::new(),
            rename_stages: HashMap::new(),
            zip_creates: HashMap::new(),
            pending_zip_create_intents: HashMap::new(),
            seven_zip_creates: HashMap::new(),
            pending_seven_zip_create_intents: HashMap::new(),
            failed_seven_zip_create_journal: HashMap::new(),
            seven_zip_create_journal_failed: HashSet::new(),
            zip_extracts: HashMap::new(),
            pending_zip_extract_intents: HashMap::new(),
            seven_zip_extract_batches: HashMap::new(),
            pending_rename_intents: HashMap::new(),
            batch_rename_completed: HashMap::new(),
            pending_batch_rename_items: HashMap::new(),
            pending_batch_undo: HashMap::new(),
            batch_undo_receipts: HashMap::new(),
            pending_batch_plan_intents: HashMap::new(),
            directory_moves: HashMap::new(),
            directory_move_replaces: HashMap::new(),
            copy_replace_cleanups: HashMap::new(),
            keep_both_directory_items: HashSet::new(),
            source_cleanup_leases: HashMap::new(),
            copy_stage_custody: HashMap::new(),
            copy_stage_cleanup_leases: HashMap::new(),
            copy_stage_cleanup_running_acks: HashMap::new(),
            #[cfg(test)]
            copy_stage_cleanup_native_observers: HashMap::new(),
            undo_ledger: Vec::new(),
            undo_reservation: None,
            created_object_undo_candidates: HashMap::new(),
            invalid_created_object_undo_candidates: HashSet::new(),
            directory_copy_undo_candidates: HashMap::new(),
            invalid_directory_copy_undo_candidates: HashSet::new(),
            ordinary_copy_source_kinds: HashMap::new(),
            ordinary_copy_source_kinds_by_job: HashMap::new(),
            ordinary_copy_source_kinds_by_job_items: HashMap::new(),
            move_undo_candidates: HashMap::new(),
            invalid_move_undo_candidates: HashSet::new(),
            next_inspection_ticket: 1,
            pending_move_inspection: None,
            inspection_history: HashMap::new(),
            latest_move_inspection: HashMap::new(),
            pending_cross_volume_move_inspection: None,
            cross_volume_move_inspection_history: HashMap::new(),
            latest_cross_volume_move_inspection: HashMap::new(),
            pending_published_move_inspection: None,
            published_move_inspection_history: HashMap::new(),
            latest_published_move_inspection: HashMap::new(),
            pending_copy_inspection: None,
            copy_inspection_history: HashMap::new(),
            latest_copy_inspection: HashMap::new(),
            returned_attempts: HashSet::new(),
            missing_terminal_candidates: HashSet::new(),
            synthesized_unknown_attempts: HashSet::new(),
        }
    }

    pub fn service(&self) -> &OperationService {
        &self.service
    }

    pub fn identity_available(&self) -> bool {
        self.run_id.is_some()
    }

    pub fn job(&self, token: AttemptToken) -> Option<JobSnapshot> {
        self.service.job(token.job_id())
    }

    /// Passive no-effect evidence for one exact terminal guarded Move attempt.
    pub fn guarded_move_no_effect_proofs(
        &self,
        token: AttemptToken,
    ) -> Option<Vec<super::model::GuardedMoveNoEffectProof>> {
        self.service.guarded_move_no_effect_proofs(token)
    }

    /// Reserves passive inspection only after the caller confirms the
    /// originating executor returned. No operation attempt is created.
    pub(crate) fn begin_guarded_move_inspection(
        &mut self,
        token: AttemptToken,
    ) -> Option<super::model::GuardedMoveInspectionResult> {
        if self.running.is_some()
            || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token)
        {
            return None;
        }
        let proofs = self.guarded_move_no_effect_proofs(token)?;
        if proofs.is_empty() {
            return None;
        }
        let ticket = super::model::InspectionTicket::new(self.next_inspection_ticket);
        self.next_inspection_ticket = self.next_inspection_ticket.checked_add(1)?;
        let work = super::model::GuardedMoveInspectionResult {
            ticket,
            attempt: token,
            candidates: proofs
                .into_iter()
                .map(|proof| super::model::GuardedMoveInspectionCandidate {
                    proof,
                    source_matches: false,
                    destination_parent_matches: false,
                    destination: super::model::GuardedMoveDestinationState::Indeterminate,
                })
                .collect(),
        };
        self.pending_move_inspection = Some(work.clone());
        self.latest_move_inspection.insert(token, ticket);
        Some(work)
    }

    pub(crate) fn begin_cross_volume_move_inspection(&mut self, token: AttemptToken) -> Option<super::model::CrossVolumeMoveInspectionResult> {
        if self.running.is_some() || !self.returned_attempts.contains(&token) || !self.is_latest_terminal(token)
            || self.pending_cross_volume_move_inspection.is_some() { return None; }
        let proofs = self.service.cross_volume_move_no_effect_proofs(token)?;
        let proofs: Vec<_> = proofs.into_iter()
            .filter(|proof| proof.source_signature.is_reliable_for_cross_volume_move_retry())
            .collect();
        if proofs.is_empty() { return None; }
        let ticket = super::model::InspectionTicket::new(self.next_inspection_ticket);
        self.next_inspection_ticket = self.next_inspection_ticket.checked_add(1)?;
        let work = super::model::CrossVolumeMoveInspectionResult {
            ticket, attempt: token,
            candidates: proofs.into_iter().map(|proof| super::model::CrossVolumeMoveInspectionCandidate {
                proof, source_matches: false, destination_parent_matches: false,
                destination: super::model::GuardedMoveDestinationState::Indeterminate,
            }).collect(),
        };
        self.pending_cross_volume_move_inspection = Some(work.clone());
        self.latest_cross_volume_move_inspection.insert(token, ticket);
        Some(work)
    }

    pub(crate) fn finish_cross_volume_move_inspection(&mut self, result: super::model::CrossVolumeMoveInspectionResult) -> bool {
        let Some(pending) = self.pending_cross_volume_move_inspection.as_ref() else { return false; };
        if pending.ticket != result.ticket || pending.attempt != result.attempt
            || pending.candidates.iter().map(|c| &c.proof).ne(result.candidates.iter().map(|c| &c.proof))
            || self.latest_cross_volume_move_inspection.get(&result.attempt) != Some(&result.ticket)
            || !self.is_latest_terminal(result.attempt) { return false; }
        self.pending_cross_volume_move_inspection = None;
        self.cross_volume_move_inspection_history.insert(result.ticket, result);
        true
    }

    pub fn latest_cross_volume_move_inspection(&self, token: AttemptToken) -> Option<(super::model::InspectionTicket, Option<&super::model::CrossVolumeMoveInspectionResult>)> {
        let ticket = *self.latest_cross_volume_move_inspection.get(&token)?;
        let result = self.cross_volume_move_inspection_history.get(&ticket);
        let pending = self.pending_cross_volume_move_inspection.as_ref().is_some_and(|work| work.ticket == ticket);
        (result.is_some() || pending).then_some((ticket, result))
    }

    pub(crate) fn begin_published_move_inspection(&mut self, token: AttemptToken) -> Option<super::model::PublishedMoveInspectionResult> {
        if self.running.is_some() || !self.returned_attempts.contains(&token) || !self.is_latest_terminal(token) || self.pending_move_inspection.is_some() || self.pending_published_move_inspection.is_some() || self.pending_copy_inspection.is_some() { return None; }
        let unknown_disposition = self.synthesized_unknown_attempts.contains(&token)
            && self.job(token).is_some_and(|job| job.attempts.last().is_some_and(|attempt| attempt.token == token && attempt.terminal.as_ref().is_some_and(|terminal| terminal.outcome == TerminalOutcome::Unknown)));
        let proofs = if unknown_disposition {
            self.service.published_move_disposition(token)?.into_iter().map(|proof| super::model::PublishedMoveSourceRetainedProof {
                item: proof.item, source: proof.source, destination: proof.destination,
                source_identity: proof.source_identity, source_signature: proof.source_signature,
                destination_parent_identity: proof.destination_parent_identity,
                destination_identity: proof.destination_identity, destination_signature: proof.destination_signature,
            }).collect()
        } else {
            self.service.published_move_source_retained(token)?
        };
        if proofs.is_empty() { return None; }
        let ticket = super::model::InspectionTicket::new(self.next_inspection_ticket);
        self.next_inspection_ticket = self.next_inspection_ticket.checked_add(1)?;
        let work = super::model::PublishedMoveInspectionResult {
            ticket, attempt: token, synthesized_unknown: unknown_disposition,
            candidates: proofs.into_iter().map(|proof| super::model::PublishedMoveInspectionCandidate {
                proof, source: super::model::PublishedMoveInspectionState::Indeterminate,
                destination_parent: super::model::PublishedMoveInspectionState::Indeterminate,
                destination: super::model::PublishedMoveInspectionState::Indeterminate,
            }).collect(),
        };
        self.pending_published_move_inspection = Some(work.clone());
        self.latest_published_move_inspection.insert(token, ticket);
        Some(work)
    }

    pub(crate) fn finish_published_move_inspection(&mut self, result: super::model::PublishedMoveInspectionResult) -> bool {
        let Some(pending) = self.pending_published_move_inspection.as_ref() else { return false; };
        if pending.ticket != result.ticket { return false; }
        let expected = pending.clone();
        self.pending_published_move_inspection = None;
        if expected.attempt != result.attempt
            || expected.synthesized_unknown != result.synthesized_unknown
            || expected.candidates.iter().map(|candidate| &candidate.proof).collect::<Vec<_>>() != result.candidates.iter().map(|candidate| &candidate.proof).collect::<Vec<_>>()
            || self.latest_published_move_inspection.get(&result.attempt) != Some(&result.ticket)
            || !self.is_latest_terminal(result.attempt) { return false; }
        self.published_move_inspection_history.insert(result.ticket, result);
        true
    }

    pub fn latest_published_move_inspection(&self, token: AttemptToken) -> Option<(super::model::InspectionTicket, Option<&super::model::PublishedMoveInspectionResult>)> {
        let ticket = *self.latest_published_move_inspection.get(&token)?;
        let result = self.published_move_inspection_history.get(&ticket);
        let pending = self.pending_published_move_inspection.as_ref().is_some_and(|work| work.ticket == ticket);
        (result.is_some() || pending).then_some((ticket, result))
    }

    pub(crate) fn mark_operation_worker_returned(&mut self, token: AttemptToken) {
        if self.is_latest_terminal(token) {
            self.returned_attempts.insert(token);
        }
    }

    pub(crate) fn finish_guarded_move_inspection(
        &mut self,
        result: super::model::GuardedMoveInspectionResult,
    ) -> bool {
        let Some(pending) = self.pending_move_inspection.as_ref() else {
            return false;
        };
        if pending.ticket != result.ticket
            || pending.attempt != result.attempt
            || pending
                .candidates
                .iter()
                .map(|candidate| &candidate.proof)
                .collect::<Vec<_>>()
                != result
                    .candidates
                    .iter()
                    .map(|candidate| &candidate.proof)
                    .collect::<Vec<_>>()
            || !self.is_latest_terminal(result.attempt)
        {
            return false;
        }
        self.pending_move_inspection = None;
        self.inspection_history.insert(result.ticket, result);
        true
    }

    pub(crate) fn cancel_guarded_move_inspection(
        &mut self,
        ticket: super::model::InspectionTicket,
    ) -> bool {
        if self
            .pending_move_inspection
            .as_ref()
            .is_some_and(|pending| pending.ticket == ticket)
        {
            self.pending_move_inspection = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn guarded_move_inspection(
        &self,
        ticket: super::model::InspectionTicket,
    ) -> Option<&super::model::GuardedMoveInspectionResult> {
        self.inspection_history.get(&ticket)
    }

    pub fn copy_failure_provenance(
        &self,
        token: AttemptToken,
    ) -> Option<Vec<super::model::CopyFailureProvenance>> {
        self.service.copy_failure_provenance(token)
    }

    pub(crate) fn begin_copy_failure_inspection(
        &mut self,
        token: AttemptToken,
    ) -> Option<super::model::CopyFailureInspectionResult> {
        if self.running.is_some()
            || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token)
            || self.pending_move_inspection.is_some()
        {
            return None;
        }
        let provenance = self.copy_failure_provenance(token)?;
        let directory_candidates = self.job(token)
            .and_then(|job| job.attempts.into_iter().find(|attempt| attempt.token == token))
            .and_then(|attempt| attempt.terminal)
            .map(|terminal| directory_copy_inspection_evidence(&terminal))
            .unwrap_or_default()
            .into_iter()
            .map(|evidence| super::model::CopyDirectoryInspectionCandidate {
                evidence,
                stage_root: super::model::CopyInspectionState::Indeterminate,
                destination_root: super::model::CopyInspectionState::Indeterminate,
            })
            .collect::<Vec<_>>();
        if provenance.is_empty() && directory_candidates.is_empty() {
            return None;
        }
        let ticket = super::model::InspectionTicket::new(self.next_inspection_ticket);
        self.next_inspection_ticket = self.next_inspection_ticket.checked_add(1)?;
        let result = super::model::CopyFailureInspectionResult {
            ticket,
            attempt: token,
            candidates: provenance
                .into_iter()
                .map(|provenance| super::model::CopyFailureInspectionCandidate {
                    provenance,
                    source: super::model::CopyInspectionState::Indeterminate,
                    destination_parent: super::model::CopyInspectionState::Indeterminate,
                    destination: super::model::CopyInspectionState::Indeterminate,
                    stage: super::model::CopyInspectionState::Indeterminate,
                    stage_content: super::model::CopyInspectionState::Indeterminate,
                })
                .collect(),
            directory_candidates,
        };
        self.pending_copy_inspection = Some(result.clone());
        self.latest_copy_inspection.insert(token, ticket);
        Some(result)
    }

    pub(crate) fn finish_copy_failure_inspection(
        &mut self,
        mut result: super::model::CopyFailureInspectionResult,
    ) -> bool {
        let Some(pending) = self.pending_copy_inspection.as_ref() else {
            return false;
        };
        if pending.ticket != result.ticket
            || pending.attempt != result.attempt
            || self.latest_copy_inspection.get(&result.attempt) != Some(&result.ticket)
            || !self.is_latest_terminal(result.attempt)
            || pending
                .candidates
                .iter()
                .map(|c| &c.provenance)
                .ne(result.candidates.iter().map(|c| &c.provenance))
            || pending.directory_candidates.iter().map(|c| &c.evidence)
                .ne(result.directory_candidates.iter().map(|c| &c.evidence))
        {
            return false;
        }
        // Stage creation succeeded, but the executor never proved which root it created.
        // A detached observation cannot repair that missing origin proof.
        for candidate in &mut result.directory_candidates {
            if matches!(
                &candidate.evidence,
                super::model::CopyDirectoryInspectionEvidence::RetainedStage(evidence)
                    if evidence.phase == super::model::CopyDirectoryStagePhase::CreatedRootIdentityUnverified
            ) {
                candidate.stage_root = super::model::CopyInspectionState::Indeterminate;
            }
        }
        self.pending_copy_inspection = None;
        self.copy_inspection_history.insert(result.ticket, result);
        true
    }

    pub(crate) fn cancel_copy_failure_inspection(
        &mut self,
        ticket: super::model::InspectionTicket,
    ) -> bool {
        if self
            .pending_copy_inspection
            .as_ref()
            .is_some_and(|pending| pending.ticket == ticket)
        {
            self.pending_copy_inspection = None;
            true
        } else {
            false
        }
    }

    pub fn latest_copy_failure_inspection(
        &self,
        token: AttemptToken,
    ) -> Option<(
        super::model::InspectionTicket,
        Option<&super::model::CopyFailureInspectionResult>,
    )> {
        let ticket = *self.latest_copy_inspection.get(&token)?;
        let result = self.copy_inspection_history.get(&ticket);
        let pending = self
            .pending_copy_inspection
            .as_ref()
            .is_some_and(|work| work.ticket == ticket);
        (result.is_some() || pending).then_some((ticket, result))
    }

    pub fn pending_guarded_move_inspection(&self, token: AttemptToken) -> bool {
        self.pending_move_inspection
            .as_ref()
            .is_some_and(|pending| pending.attempt == token)
    }

    pub fn latest_guarded_move_inspection(
        &self,
        token: AttemptToken,
    ) -> Option<(
        super::model::InspectionTicket,
        Option<&super::model::GuardedMoveInspectionResult>,
    )> {
        let ticket = *self.latest_move_inspection.get(&token)?;
        let result = self.inspection_history.get(&ticket);
        let pending = self
            .pending_move_inspection
            .as_ref()
            .is_some_and(|work| work.ticket == ticket);
        (result.is_some() || pending).then_some((ticket, result))
    }

    /// Admits only candidates backed by the latest completed exact inspection and terminal facts.
    pub fn retry_verified_guarded_move_items(
        &mut self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
    ) -> Result<AttemptToken, super::model::RetryError> {
        let retry_items = self
            .verified_guarded_move_retry_items(token, ticket)
            .filter(|items| !items.is_empty())
            .ok_or(super::model::RetryError::NotRetryable)?;
        self.service.retry_verified_items(token, retry_items)
    }

    pub fn retry_verified_cross_volume_move_items(&mut self, token: AttemptToken, ticket: super::model::InspectionTicket) -> Result<AttemptToken, super::model::RetryError> {
        let guards = self.verified_cross_volume_move_retry_guards(token, ticket)
            .filter(|guards| !guards.is_empty()).ok_or(super::model::RetryError::NotRetryable)?;
        self.service.retry_verified_cross_volume_move_items(token, guards)
    }

    pub fn verified_cross_volume_move_retry_guards(&self, token: AttemptToken, ticket: super::model::InspectionTicket) -> Option<Vec<super::model::CrossVolumeMoveRetryGuard>> {
        if self.run_id.is_none() || self.running.is_some() || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token) || self.latest_cross_volume_move_inspection.get(&token) != Some(&ticket)
            || self.pending_cross_volume_move_inspection.is_some() { return None; }
        let inspection = self.cross_volume_move_inspection_history.get(&ticket).filter(|r| r.attempt == token)?;
        let job = self.service.job(token.job_id())?;
        if job.request.kind != super::model::OperationKind::Move || job.request.virtual_source.is_some()
            || job.request.move_destination_identity_guard.is_some() { return None; }
        let attempt = job.attempts.last()?;
        let report = attempt.terminal.as_ref()?;
        if !matches!(report.outcome, super::model::TerminalOutcome::Failed | super::model::TerminalOutcome::Partial | super::model::TerminalOutcome::Cancelled)
            || report.retry != super::model::RetryPlan::RequiresReinspection { return None; }
        let mut eligible = Vec::new();
        let mut seen = HashSet::new();
        for candidate in &inspection.candidates {
            let proof = &candidate.proof;
            if !seen.insert(proof.item) || !candidate.source_matches || !candidate.destination_parent_matches
                || candidate.destination != super::model::GuardedMoveDestinationState::Absent
                || proof.source_identity.windows_parts().is_none() || proof.destination_parent_identity.windows_parts().is_none()
                || !proof.source_signature.is_reliable_for_cross_volume_move_retry() { continue; }
            let matching: Vec<_> = report.receipts.iter().filter(|receipt| receipt.item == proof.item).collect();
            if matching.len() != 1 { continue; }
            let receipt = matching[0];
            let receipt_ok = match receipt.outcome {
                super::model::ItemOutcome::Failed => receipt.move_receipt.as_ref().is_none_or(|m|
                    m.source == proof.source && m.destination == proof.destination && !m.destination_published && !m.source_removed),
                super::model::ItemOutcome::CancelledBeforeStart => receipt.move_receipt.as_ref().is_none_or(|m|
                    m.source == proof.source && m.destination == proof.destination && !m.destination_published && !m.source_removed),
                _ => false,
            };
            if !receipt_ok || receipt.replace_receipt.is_some() || receipt.created_folder_path.is_some()
                || receipt.created_archive_path.is_some() || receipt.copy_undo.is_some() { continue; }
            eligible.push(super::model::CrossVolumeMoveRetryGuard { proof: proof.clone() });
        }
        Some(eligible)
    }

    /// Disposable owner projection of the exact items the guarded Move confirmation could admit.
    pub fn verified_guarded_move_retry_items(
        &self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
    ) -> Option<Vec<super::model::ItemId>> {
        if self.run_id.is_none()
            || self.running.is_some()
            || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token)
            || self.latest_move_inspection.get(&token) != Some(&ticket)
            || self.pending_move_inspection.is_some()
        {
            return None;
        }
        let inspection = self
            .inspection_history
            .get(&ticket)
            .filter(|result| result.attempt == token)?;
        let job = self.service.job(token.job_id())?;
        if job.request.kind != super::model::OperationKind::Move {
            return None;
        }
        let attempt = job.attempts.last()?;
        let report = attempt.terminal.as_ref()?;
        if report.outcome == super::model::TerminalOutcome::Succeeded
            || report.retry != super::model::RetryPlan::RequiresReinspection
        {
            return None;
        }

        // Build counts once so any duplicate candidate or terminal receipt fails closed.
        let mut candidate_counts = HashMap::new();
        for candidate in &inspection.candidates {
            *candidate_counts
                .entry(candidate.proof.item)
                .or_insert(0usize) += 1;
        }
        let mut receipt_counts = HashMap::new();
        let mut receipts = HashMap::new();
        for receipt in &report.receipts {
            *receipt_counts.entry(receipt.item).or_insert(0usize) += 1;
            receipts.entry(receipt.item).or_insert(receipt);
        }
        let mut retry_items = Vec::new();
        let mut retry_set = HashSet::new();
        for candidate in &inspection.candidates {
            let proof = &candidate.proof;
            if candidate_counts.get(&proof.item) != Some(&1)
                || receipt_counts.get(&proof.item) != Some(&1)
            {
                continue;
            }
            let Some(receipt) = receipts.get(&proof.item).copied() else {
                continue;
            };
            // 直接のno-effect proofと同じpath・false/falseのreceiptだけは、失敗前の記録として両立させる。
            let move_receipt_is_proven_no_effect =
                receipt.move_receipt.as_ref().is_none_or(|move_receipt| {
                    receipt.outcome == super::model::ItemOutcome::Failed
                        && !move_receipt.destination_published
                        && !move_receipt.source_removed
                        && move_receipt.source == proof.source
                        && move_receipt.destination == proof.destination
                });
            if !matches!(
                receipt.outcome,
                super::model::ItemOutcome::Failed | super::model::ItemOutcome::CancelledBeforeStart
            ) || !move_receipt_is_proven_no_effect
                || receipt.replace_receipt.is_some()
                || receipt.created_folder_path.is_some()
                || receipt.created_archive_path.is_some()
                || receipt.copy_undo.is_some()
                || !candidate.source_matches
                || !candidate.destination_parent_matches
                || candidate.destination != super::model::GuardedMoveDestinationState::Absent
            {
                continue;
            }
            if retry_set.insert(proof.item) {
                retry_items.push(proof.item);
            }
        }
        Some(retry_items)
    }

    /// Admits ordinary-file Copy retry only from exact, current NotPublished evidence.
    pub fn retry_verified_copy_items(
        &mut self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
    ) -> Result<AttemptToken, super::model::RetryError> {
        let file_guards = self.verified_copy_retry_guards(token, ticket)
            .ok_or(super::model::RetryError::NotRetryable)?;
        let directory_guards = self.verified_copy_directory_retry_guards(token, ticket)
            .ok_or(super::model::RetryError::NotRetryable)?;
        if file_guards.is_empty() && directory_guards.is_empty() { return Err(super::model::RetryError::NotRetryable); }
        let (guarded_file_classification, guarded_directory_classification) = if let Some(job) = self.service.job(token.job_id()) {
            let classification = self.ordinary_copy_source_kinds_by_job.get(&token.job_id());
            if job.request.copy_destination_identity_guard.is_none() {
                (None, None)
            } else {
                let mut files = Vec::new();
                let mut directories = Vec::new();
                for index in 0..job.request.sources.len() {
                    let item = super::model::ItemId(u32::try_from(index).map_err(|_| super::model::RetryError::NotRetryable)?);
                    match self.ordinary_copy_source_kinds_by_job_items.get(&token.job_id())
                        .and_then(|kinds| kinds.get(index)) {
                        Some(super::executor::OrdinaryCopySourceKind::File) => files.push(item),
                        Some(super::executor::OrdinaryCopySourceKind::Directory) => directories.push(item),
                        Some(super::executor::OrdinaryCopySourceKind::Indeterminate) => {
                            return Err(super::model::RetryError::NotRetryable);
                        }
                        None => return Err(super::model::RetryError::NotRetryable),
                    }
                }
                match classification {
                    Some(OrdinaryCopyRequestClassification::AllFiles) if directories.is_empty() => (Some(files), Some(directories)),
                    Some(OrdinaryCopyRequestClassification::AllDirectories) if files.is_empty() => (Some(files), Some(directories)),
                    Some(OrdinaryCopyRequestClassification::Mixed) if !files.is_empty() && !directories.is_empty() => (Some(files), Some(directories)),
                    _ => return Err(super::model::RetryError::NotRetryable),
                }
            }
        } else { return Err(super::model::RetryError::NotRetryable); };
        self.service.retry_verified_copy_items_with_directory_guards(
            token, file_guards, directory_guards, guarded_file_classification, guarded_directory_classification,
        )
    }

    pub fn verified_copy_directory_retry_guards(
        &self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
    ) -> Option<Vec<super::model::CopyDirectoryRetryGuard>> {
        if self.run_id.is_none() || self.running.is_some() || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token) || self.latest_copy_inspection.get(&token) != Some(&ticket)
            || self.pending_copy_inspection.is_some() { return None; }
        let inspection = self.copy_inspection_history.get(&ticket).filter(|result| result.attempt == token)?;
        let job = self.service.job(token.job_id())?;
        if job.request.kind != super::model::OperationKind::Copy || job.request.virtual_source.is_some() { return None; }
        let attempt = job.attempts.last()?;
        if attempt.token != token { return None; }
        let is_original_attempt = job.attempts.len() == 1
            && attempt.token == token
            && attempt.retry_items.is_empty();
        let report = attempt.terminal.as_ref()?;
        if !matches!(report.outcome, super::model::TerminalOutcome::Failed | super::model::TerminalOutcome::Partial)
            || report.retry != super::model::RetryPlan::RequiresReinspection { return None; }
        let destination_identity_guard = job.request.copy_destination_identity_guard.as_ref();
        if destination_identity_guard.is_some() {
            match self.ordinary_copy_source_kinds_by_job.get(&token.job_id()) {
                Some(OrdinaryCopyRequestClassification::AllFiles) => return Some(Vec::new()),
                Some(OrdinaryCopyRequestClassification::AllDirectories | OrdinaryCopyRequestClassification::Mixed) => {}
                _ => return None,
            }
        }
        let mut provenance_counts = HashMap::new();
        for provenance in &attempt.copy_directory_retry_provenance {
            *provenance_counts.entry(provenance.item).or_insert(0usize) += 1;
        }
        let mut guards = Vec::new();
        for provenance in &attempt.copy_directory_retry_provenance {
            let item = provenance.item;
            let original_kind_is_directory = is_original_attempt
                && self.ordinary_copy_source_kinds_by_job_items.get(&token.job_id())
                    .and_then(|kinds| kinds.get(item.0 as usize))
                    == Some(&super::executor::OrdinaryCopySourceKind::Directory);
            let current_kind_is_directory = self.ordinary_copy_source_kinds.get(&(token, item))
                == Some(&super::executor::OrdinaryCopySourceKind::Directory)
                || original_kind_is_directory
                || (destination_identity_guard.is_some()
                    && attempt.retry_items.contains(&item)
                    && matches!(self.ordinary_copy_source_kinds_by_job.get(&token.job_id()),
                        Some(OrdinaryCopyRequestClassification::AllDirectories | OrdinaryCopyRequestClassification::Mixed))
                    && attempt.copy_directory_retry_guards.iter().any(|guard| {
                        guard.item == item && guard.source == provenance.source
                            && guard.destination == provenance.destination
                    }));
            if provenance_counts.get(&item) != Some(&1)
                || !current_kind_is_directory
                || attempt.invalid_copy_directory_retry_items.contains(&item) { continue; }
            let receipt_matches = report.receipts.iter().filter(|receipt| receipt.item == item).collect::<Vec<_>>();
            let [receipt] = receipt_matches.as_slice() else { continue; };
            if !matches!(receipt.outcome, super::model::ItemOutcome::Failed | super::model::ItemOutcome::Partial)
                || receipt.move_receipt.is_some() || receipt.replace_receipt.is_some()
                || receipt.created_folder_path.is_some() || receipt.created_archive_path.is_some()
                || receipt.copy_undo.is_some() { continue; }
            // Candidate paths alone do not identify an item: duplicate basenames in one
            // request would map multiple ItemIds onto the same destination evidence.
            let Some(requested_leaf) = provenance.source.file_name() else { continue; };
            if job.request.sources.iter().filter_map(|source| source.file_name())
                .filter(|leaf| windows_ordinal_leaf_eq(leaf, requested_leaf)).count() != 1 { continue; }
            // The terminal receipt is the item-bound authority. Inspection must reproduce
            // that exact retained-stage record, including its phase and identities.
            let Some(detail) = receipt.detail.as_deref() else { continue; };
            if detail.matches(super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX).count() != 1
                || detail.contains(super::model::DIRECTORY_COPY_EVIDENCE_PREFIX) { continue; }
            let Some((_, payload)) = detail.split_once(super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX) else { continue; };
            let Ok(receipt_stage_evidence) = serde_json::from_str::<super::model::CopyDirectoryStageEvidence>(payload) else { continue; };
            if receipt_stage_evidence.phase != super::model::CopyDirectoryStagePhase::StagingFailed { continue; }
            #[cfg(windows)]
            let expected_destination_units = {
                use std::os::windows::ffi::OsStrExt;
                provenance.destination.as_os_str().encode_wide().collect::<Vec<_>>()
            };
            #[cfg(not(windows))]
            let expected_destination_units: Vec<u16> = Vec::new();
            // Inspection has no item ID for these rows; the exact native destination path
            // is the only safe key back to this item's provenance. Count every evidence kind
            // at that path so a conflicting publication row cannot hide beside a stage row.
            let matching = inspection.directory_candidates.iter().filter(|candidate| {
                match &candidate.evidence {
                    super::model::CopyDirectoryInspectionEvidence::RetainedStage(evidence) =>
                        !expected_destination_units.is_empty()
                            && evidence.destination_path_units == expected_destination_units,
                    super::model::CopyDirectoryInspectionEvidence::Publication(evidence) =>
                        !expected_destination_units.is_empty()
                            && evidence.destination_path_units == expected_destination_units,
                }
            }).collect::<Vec<_>>();
            if !matches!(matching.as_slice(), [candidate]
                if matches!(&candidate.evidence,
                    super::model::CopyDirectoryInspectionEvidence::RetainedStage(evidence)
                        if evidence == &receipt_stage_evidence)
                    && candidate.destination_root == super::model::CopyInspectionState::Absent
                    && candidate.stage_root != super::model::CopyInspectionState::Indeterminate) { continue; }
            let Some(source) = job.request.sources.get(item.0 as usize) else { continue; };
            if source != &provenance.source
                || job.request.destination.as_deref() != provenance.destination.parent()
                || provenance.source.file_name() != provenance.destination.file_name()
                || job.request.destination.as_ref().zip(source.file_name())
                    .map(|(parent, leaf)| parent.join(leaf)).as_ref() != Some(&provenance.destination)
                || provenance.source_root_identity.windows_parts().is_none()
                || provenance.destination_parent_identity.windows_parts().is_none()
                || destination_identity_guard.is_some_and(|guard| guard != &provenance.destination_parent_identity)
            { continue; }
            guards.push(super::model::CopyDirectoryRetryGuard {
                item, source: provenance.source.clone(), destination: provenance.destination.clone(),
                source_root_identity: provenance.source_root_identity.clone(),
                destination_parent_identity: provenance.destination_parent_identity.clone(),
            });
        }
        Some(guards)
    }

    /// Projects exact worker guards for items eligible for a confirmed Copy retry.
    pub fn verified_copy_retry_guards(
        &self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
    ) -> Option<Vec<super::model::CopyRetryGuard>> {
        if self.run_id.is_none()
            || self.running.is_some()
            || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token)
            || self.latest_copy_inspection.get(&token) != Some(&ticket)
            || self.pending_copy_inspection.is_some()
        {
            return None;
        }
        let inspection = self
            .copy_inspection_history
            .get(&ticket)
            .filter(|result| result.attempt == token)?;
        let job = self.service.job(token.job_id())?;
        if job.request.kind != super::model::OperationKind::Copy
            || job.request.virtual_source.is_some()
        {
            return None;
        }
        let attempt = job.attempts.last()?;
        if attempt.token != token {
            return None;
        }
        let is_original_attempt = job.attempts.len() == 1
            && attempt.token == token
            && attempt.retry_items.is_empty();
        let report = attempt.terminal.as_ref()?;
        if !matches!(
            report.outcome,
            super::model::TerminalOutcome::Failed | super::model::TerminalOutcome::Partial
        ) || report.retry != super::model::RetryPlan::RequiresReinspection
        {
            return None;
        }
        let destination_identity_guard = job.request.copy_destination_identity_guard.as_ref();
        if destination_identity_guard.is_some() {
            match self.ordinary_copy_source_kinds_by_job.get(&token.job_id()) {
                Some(OrdinaryCopyRequestClassification::AllFiles | OrdinaryCopyRequestClassification::Mixed) => {}
                Some(OrdinaryCopyRequestClassification::AllDirectories) => return Some(Vec::new()),
                _ => return None,
            }
        }
        let authoritative_provenance = self.copy_failure_provenance(token)?;
        let mut candidate_counts = HashMap::new();
        for candidate in &inspection.candidates {
            *candidate_counts
                .entry(candidate.provenance.item)
                .or_insert(0usize) += 1;
        }
        let mut receipt_counts = HashMap::new();
        let mut receipts = HashMap::new();
        for receipt in &report.receipts {
            *receipt_counts.entry(receipt.item).or_insert(0usize) += 1;
            receipts.entry(receipt.item).or_insert(receipt);
        }
        let mut guards = Vec::new();
        let mut seen = HashSet::new();
        for candidate in &inspection.candidates {
            let provenance = &candidate.provenance;
            let item = provenance.item;
            if candidate_counts.get(&item) != Some(&1)
                || receipt_counts.get(&item) != Some(&1)
                || !seen.insert(item)
                || attempt.invalid_copy_failure_retry_items.contains(&item)
            {
                continue;
            }
            let item_provenance = authoritative_provenance.iter()
                .filter(|current| current.item == item).collect::<Vec<_>>();
            if !matches!(item_provenance.as_slice(), [current] if *current == &candidate.provenance) {
                continue;
            }
            let original_kind_is_file = is_original_attempt
                && self.ordinary_copy_source_kinds_by_job_items.get(&token.job_id())
                    .and_then(|kinds| kinds.get(item.0 as usize))
                    == Some(&super::executor::OrdinaryCopySourceKind::File);
            let current_kind_is_file = self.ordinary_copy_source_kinds.get(&(token, item))
                == Some(&super::executor::OrdinaryCopySourceKind::File)
                || original_kind_is_file
                || (destination_identity_guard.is_none()
                    && self.ordinary_copy_source_kinds.get(&(token, item))
                        != Some(&super::executor::OrdinaryCopySourceKind::Directory))
                || (destination_identity_guard.is_some()
                    && attempt.retry_items.contains(&item)
                    && matches!(self.ordinary_copy_source_kinds_by_job.get(&token.job_id()),
                        Some(OrdinaryCopyRequestClassification::AllFiles | OrdinaryCopyRequestClassification::Mixed))
                    && attempt.copy_retry_guards.iter().any(|guard| {
                        guard.item == item && guard.source == candidate.provenance.source
                            && guard.destination == candidate.provenance.destination
                    }));
            if !current_kind_is_file { continue; }
            let Some(receipt) = receipts.get(&item).copied() else {
                continue;
            };
            if !matches!(
                receipt.outcome,
                super::model::ItemOutcome::Failed | super::model::ItemOutcome::Partial
            )
                || receipt.move_receipt.is_some()
                || receipt.replace_receipt.is_some()
                || receipt.created_folder_path.is_some()
                || receipt.created_archive_path.is_some()
                || receipt.copy_undo.is_some()
                || provenance.publication != super::model::CopyPublicationState::NotPublished
                || candidate.source != super::model::CopyInspectionState::Matches
                || candidate.destination_parent != super::model::CopyInspectionState::Matches
                || candidate.destination != super::model::CopyInspectionState::Absent
                || provenance.source_signature_semantics
                    != super::model::CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle
            {
                continue;
            }
            let (Some(source_identity), Some(source_signature), Some(destination_parent_identity)) = (
                provenance.source_identity.clone(),
                provenance.source_signature,
                provenance.destination_parent_identity.clone(),
            ) else {
                continue;
            };
            let Some(source_path) = job.request.sources.get(item.0 as usize) else {
                continue;
            };
            let Some(requested_leaf) = source_path.file_name() else { continue; };
            let destination_mapping_is_unique = job.request.sources.iter().filter_map(|source| source.file_name())
                .filter(|leaf| windows_ordinal_leaf_eq(leaf, requested_leaf)).count() == 1;
            if source_path != &provenance.source
                || job.request.destination.as_deref() != provenance.destination.parent()
                || source_path.file_name() != provenance.destination.file_name()
                || job.request.destination.as_ref().zip(source_path.file_name())
                    .map(|(parent, leaf)| parent.join(leaf)).as_ref() != Some(&provenance.destination)
                || !destination_mapping_is_unique
                || source_identity.windows_parts().is_none()
                || destination_parent_identity.windows_parts().is_none()
                || destination_identity_guard.is_some_and(|guard| guard != &destination_parent_identity)
            {
                continue;
            }
            guards.push(super::model::CopyRetryGuard {
                item,
                source: provenance.source.clone(),
                destination: provenance.destination.clone(),
                source_identity,
                source_signature,
                destination_parent_identity,
            });
        }
        Some(guards)
    }

    /// Moves custody from the returned worker into this process-local owner.
    /// A path or a passive inspection record cannot manufacture this authority.
    pub(crate) fn adopt_returned_copy_stage_custody(
        &mut self,
        custody: CopyStageCustody,
    ) -> bool {
        if self.run_id.is_none()
            || !self.returned_attempts.contains(&custody.token)
            || self.running.is_some()
            || self.copy_stage_custody.len() >= 64
            || self
                .copy_stage_custody
                .contains_key(&(custody.token, custody.item))
        {
            return false;
        }
        let Some(job) = self.service.job(custody.token.job_id()) else {
            return false;
        };
        if job.request.kind != super::model::OperationKind::Copy
            || job.request.virtual_source.is_some()
            || job.request.copy_destination_identity_guard.is_some()
        {
            return false;
        }
        let Some(attempt) = job.attempts.last() else {
            return false;
        };
        let Some(report) = attempt.terminal.as_ref() else {
            return false;
        };
        if !matches!(report.outcome, super::model::TerminalOutcome::Failed | super::model::TerminalOutcome::Partial)
            || report.retry != super::model::RetryPlan::RequiresReinspection
        {
            return false;
        }
        let Some(receipt) = report.receipts.iter().find(|receipt| receipt.item == custody.item) else {
            return false;
        };
        let Some(provenance) = self
            .service
            .copy_failure_provenance(custody.token)
            .and_then(|items| items.into_iter().find(|item| item.item == custody.item))
        else {
            return false;
        };
        let exact_stage = provenance.stage.as_ref().is_some_and(|stage| {
            stage.path == custody.path && stage.identity.as_ref() == Some(&custody.identity)
        });
        if !matches!(receipt.outcome, super::model::ItemOutcome::Failed | super::model::ItemOutcome::Partial)
            || receipt.move_receipt.is_some()
            || receipt.replace_receipt.is_some()
            || receipt.created_folder_path.is_some()
            || receipt.created_archive_path.is_some()
            || receipt.copy_undo.is_some()
            || provenance.publication != super::model::CopyPublicationState::NotPublished
            || !exact_stage
            || provenance.destination_parent_identity.as_ref() != Some(&custody.parent_identity)
            || custody.identity.windows_parts().is_none()
            || custody.parent_identity.windows_parts().is_none()
        {
            return false;
        }
        self.copy_stage_custody
            .insert((custody.token, custody.item), custody);
        true
    }

    /// Projects only exact inspected items that still own their original locked stage handle.
    pub fn verified_copy_stage_cleanup_items(
        &self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
    ) -> Option<Vec<super::model::ItemId>> {
        if self.run_id.is_none()
            || self.running.is_some()
            || !self.returned_attempts.contains(&token)
            || !self.is_latest_terminal(token)
            || self.latest_copy_inspection.get(&token) != Some(&ticket)
            || self.pending_copy_inspection.is_some()
        {
            return None;
        }
        let inspection = self.copy_inspection_history.get(&ticket)
            .filter(|result| result.attempt == token)?;
        let job = self.service.job(token.job_id())?;
        if job.request.kind != super::model::OperationKind::Copy
            || job.request.virtual_source.is_some()
            || job.request.copy_destination_identity_guard.is_some()
        {
            return None;
        }
        let attempt = job.attempts.last()?;
        let report = attempt.terminal.as_ref()?;
        if !matches!(report.outcome, super::model::TerminalOutcome::Failed | super::model::TerminalOutcome::Partial)
            || report.retry != super::model::RetryPlan::RequiresReinspection
        {
            return None;
        }
        let mut candidate_counts = HashMap::new();
        for candidate in &inspection.candidates {
            *candidate_counts.entry(candidate.provenance.item).or_insert(0usize) += 1;
        }
        let mut receipt_counts = HashMap::new();
        let mut receipts = HashMap::new();
        for receipt in &report.receipts {
            *receipt_counts.entry(receipt.item).or_insert(0usize) += 1;
            receipts.entry(receipt.item).or_insert(receipt);
        }
        let mut eligible = Vec::new();
        for candidate in &inspection.candidates {
            let item = candidate.provenance.item;
            if candidate_counts.get(&item) != Some(&1)
                || receipt_counts.get(&item) != Some(&1)
                || candidate.source != super::model::CopyInspectionState::Matches
                || candidate.destination_parent != super::model::CopyInspectionState::Matches
                || candidate.destination != super::model::CopyInspectionState::Absent
                || candidate.stage != super::model::CopyInspectionState::Matches
                || candidate.provenance.publication != super::model::CopyPublicationState::NotPublished
            {
                continue;
            }
            let Some(receipt) = receipts.get(&item).copied() else { continue; };
            if !matches!(receipt.outcome, super::model::ItemOutcome::Failed | super::model::ItemOutcome::Partial)
                || receipt.move_receipt.is_some()
                || receipt.replace_receipt.is_some()
                || receipt.created_folder_path.is_some()
                || receipt.created_archive_path.is_some()
                || receipt.copy_undo.is_some()
            {
                continue;
            }
            let Some(custody) = self.copy_stage_custody.get(&(token, item)) else { continue; };
            let exact = candidate.provenance.stage.as_ref().is_some_and(|stage| {
                stage.path == custody.path && stage.identity.as_ref() == Some(&custody.identity)
            }) && candidate.provenance.destination_parent_identity.as_ref() == Some(&custody.parent_identity);
            if exact { eligible.push(item); }
        }
        Some(eligible)
    }

    /// Consumes exact inspection authorization and the original capability atomically.
    pub fn cleanup_verified_copy_stage(
        &mut self,
        token: AttemptToken,
        ticket: super::model::InspectionTicket,
        item: super::model::ItemId,
    ) -> Result<AttemptToken, super::model::SubmitError> {
        if !self.verified_copy_stage_cleanup_items(token, ticket).is_some_and(|items| items.contains(&item)) {
            return Err(super::model::SubmitError::InvalidRequest);
        }
        let custody = self.copy_stage_custody.remove(&(token, item))
            .ok_or(super::model::SubmitError::InvalidRequest)?;
        let request = super::model::OperationRequest {
            kind: super::model::OperationKind::CopyStageCleanup,
            virtual_source: None,
            sources: vec![custody.path.clone()],
            trash_source_guards: None,
            destination: None,
            batch_rename_plan: None,
            move_identity_guard: None,
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
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
        let cleanup_token = match self.service.submit(request) {
            Ok(token) => token,
            Err(error) => {
                self.copy_stage_custody.insert((token, item), custody);
                return Err(error);
            }
        };
        self.copy_stage_cleanup_leases.insert(cleanup_token, CopyStageCleanupLease {
            token: custody.token,
            item: custody.item,
            path: custody.path,
            identity: custody.identity,
            parent_identity: custody.parent_identity,
            handle: custody.handle,
            #[cfg(test)]
            native_disposition_observer: self.copy_stage_cleanup_native_observers.remove(&token),
        });
        Ok(cleanup_token)
    }

    #[cfg(test)]
    pub fn observe_copy_stage_cleanup_native_disposition_for_test(
        &mut self,
        token: AttemptToken,
        observer: async_channel::Sender<Result<(), u32>>,
    ) {
        self.copy_stage_cleanup_native_observers.insert(token, observer);
    }

    fn is_latest_terminal(&self, token: AttemptToken) -> bool {
        self.service.job(token.job_id()).is_some_and(|job| {
            job.attempts.last().is_some_and(|attempt| {
                attempt.token == token
                    && attempt.state == super::model::AttemptState::Terminal
                    && attempt.terminal.is_some()
            })
        })
    }

    /// Immutable job history for presentation. Lifecycle mutation remains
    /// behind runtime commands and exact attempt tokens.
    pub fn jobs(&self) -> Vec<JobSnapshot> {
        self.service.jobs()
    }

    /// Immutable newest-first history bounded before snapshots are cloned.
    pub fn recent_jobs(&self, limit: usize) -> Vec<JobSnapshot> {
        self.service.recent_jobs(limit)
    }

    /// Compact live progress projection; terminal successes/cancellations are
    /// filtered by the authoritative service before any presentation clone.
    pub fn progress_rows(&self) -> Vec<super::model::ProgressRowSnapshot> {
        self.service.progress_rows()
    }

    pub fn has_progress_rows(&self) -> bool {
        self.service.has_progress_rows()
    }

    /// Lets a disposable presentation projection observe only progress that
    /// the authoritative owner accepted for the current attempt.
    pub fn commit_progress(&mut self, token: AttemptToken, update: &ProgressUpdate) -> bool {
        self.service.progress(token, update.clone())
    }

    /// Disposable presentation projection for the one worker currently holding
    /// the serial lane.  It does not give UI any mutable lifecycle authority.
    pub fn active_job(&self) -> Option<JobSnapshot> {
        self.running.and_then(|token| self.job(token))
    }

    /// Build a conservative terminal fact when a claimed worker returns without
    /// reporting one. The Shell still commits this through the ordinary fact path.
    pub fn missing_worker_terminal_candidate(&mut self, token: AttemptToken) -> Option<ExecutorFact> {
        if self.running != Some(token) {
            return None;
        }
        let job = self.job(token)?;
        let attempt = job.attempts.last()?;
        if attempt.token != token
            || !matches!(
                attempt.state,
                super::model::AttemptState::Preparing
                    | super::model::AttemptState::Running
                    | super::model::AttemptState::WaitingConflict
                    | super::model::AttemptState::CancelRequested
            )
        {
            return None;
        }
        let has_disposition = self.service.published_move_disposition(token).is_some_and(|proofs| !proofs.is_empty());
        if job.request.kind == super::model::OperationKind::Move && has_disposition { self.missing_terminal_candidates.insert(token); }
        Some(ExecutorFact::Terminal {
            token,
            report: super::model::TerminalReport {
                outcome: TerminalOutcome::Unknown,
                final_counters: attempt
                    .progress
                    .as_ref()
                    .map(|progress| progress.counters)
                    .unwrap_or_default(),
                receipts: Vec::new(),
                retry: super::model::RetryPlan::RequiresReinspection,
                change_hints: unknown_operation_change_hints(&job.request),
            },
        })
    }

    pub fn journal_attempt(&self, token: AttemptToken) -> Option<super::journal::JournalAttempt> {
        let run_id = self.run_id.as_ref()?;
        let job = self.job(token)?;
        let attempt = job.attempts.iter().find(|attempt| attempt.token == token)?;
        let mut record = super::journal::attempt_from_snapshot_for_run(&job, attempt, run_id);
        record.replace_backup = self.replace_backups.get(&token).cloned();
        record.cross_volume_move_replace = self.cross_volume_move_replaces.get(&token).cloned();
        record.directory_move_replace_rename = self.directory_move_replace_rename.get(&token).cloned();
        record.rename_stage = self.rename_stages.get(&token).cloned();
        record.zip_create = self.zip_creates.get(&token).cloned();
        record.seven_zip_create_handles = self
            .pending_seven_zip_create_intents
            .get(&token)
            .or_else(|| self.seven_zip_creates.get(&token))
            .or_else(|| self.failed_seven_zip_create_journal.get(&token))
            .cloned();
        let is_zip_extract = job.request.kind == super::model::OperationKind::ExtractZip;
        let singleton_extract =
            is_zip_extract
                && job.request.zip_create_guard.as_ref().is_some_and(|guard| {
                    matches!(guard, super::model::ZipOperationGuard::Extract(_))
                });
        record.zip_extract = singleton_extract
            .then(|| {
                self.zip_extracts
                    .get(&(token, super::model::ItemId(0)))
                    .cloned()
            })
            .flatten();
        record.zip_extract_items = self
            .zip_extracts
            .iter()
            .filter(|((attempt_token, _item), _)| {
                *attempt_token == token && is_zip_extract && !singleton_extract
            })
            .map(
                |((_, item), ledger)| super::journal::JournalZipExtractItem {
                    item: item.0,
                    ledger: ledger.clone(),
                },
            )
            .collect();
        record.zip_extract_items.sort_by_key(|item| item.item);
        if is_zip_extract && !singleton_extract {
            if let Some(report) = &attempt.terminal {
                record.zip_extract_receipts = report
                    .receipts
                    .iter()
                    .map(|receipt| super::journal::JournalZipExtractReceipt {
                        item: receipt.item.0,
                        outcome: match receipt.outcome {
                            super::model::ItemOutcome::Completed => {
                                super::journal::JournalZipExtractItemOutcome::Completed
                            }
                            super::model::ItemOutcome::Failed
                            | super::model::ItemOutcome::Skipped => {
                                super::journal::JournalZipExtractItemOutcome::Failed
                            }
                            super::model::ItemOutcome::CancelledBeforeStart => {
                                super::journal::JournalZipExtractItemOutcome::CancelledBeforeStart
                            }
                            super::model::ItemOutcome::Partial => {
                                super::journal::JournalZipExtractItemOutcome::Partial
                            }
                            super::model::ItemOutcome::Unknown => {
                                super::journal::JournalZipExtractItemOutcome::Unknown
                            }
                        },
                        output: receipt
                            .created_folder_path
                            .as_deref()
                            .map(super::journal::JournalPath::from_path),
                    })
                    .collect();
            }
        }
        if job.request.kind == super::model::OperationKind::Extract7z {
            if let Some(frozen) = self
                .seven_zip_extract_batches
                .get(&token)
                .and_then(|state| state.frozen_terminal_record.as_ref())
            {
                return Some(frozen.clone());
            }
            record.seven_zip_extract_batch =
                self.seven_zip_extract_batches
                    .get(&token)
                    .and_then(|state| {
                        state
                            .pending
                            .as_ref()
                            .or(state.failed.as_ref())
                            .or(state.durable.as_ref())
                            .cloned()
                    });
            let terminal_was_acknowledged = self
                .seven_zip_extract_batches
                .get(&token)
                .is_some_and(|state| state.terminal_journal_acknowledged);
            if !terminal_was_acknowledged && let Some(report) = &attempt.terminal {
                use super::journal::{
                    JournalSevenZipExtractOutcome as Outcome,
                    JournalSevenZipExtractTerminalReceipt as Receipt,
                };
                use super::model::ItemOutcome;
                record.seven_zip_extract_receipts = (0..job.request.sources.len()).map(|ordinal| {
                    let item = super::model::ItemId(ordinal as u32);
                    let source_receipt = report.receipts.iter().find(|receipt| receipt.item == item);
                    let ledger = record.seven_zip_extract_batch.as_ref().and_then(|batch| batch.items.iter().find(|ledger| ledger.binding.item == item.0));
                    let published = ledger.filter(|ledger| ledger.phase == super::journal::JournalSevenZipExtractPhase::Published
                        && ledger.tree_state == super::journal::JournalSevenZipExtractObjectState::Published
                        && ledger.publication_identity.as_ref() == ledger.tree_identity.as_ref());
                    let observed_publication = ledger.filter(|ledger| ledger.publication_identity.is_some());
                    let discarded = ledger.is_some_and(|ledger| ledger.phase == super::journal::JournalSevenZipExtractPhase::Discarded
                        && matches!(ledger.spool_state, super::journal::JournalSevenZipExtractObjectState::Deleted | super::journal::JournalSevenZipExtractObjectState::CreateFailed | super::journal::JournalSevenZipExtractObjectState::NeverCreated)
                        && matches!(ledger.tree_state, super::journal::JournalSevenZipExtractObjectState::Deleted | super::journal::JournalSevenZipExtractObjectState::CreateFailed | super::journal::JournalSevenZipExtractObjectState::NeverCreated));
                    let (outcome, output, publication_identity) = match source_receipt.map(|r| r.outcome) {
                        Some(ItemOutcome::Completed) if published.is_some() => {
                            let ledger = published.unwrap();
                            (Outcome::Completed, Some(ledger.binding.output.clone()), ledger.publication_identity.clone())
                        }
                        Some(ItemOutcome::Completed) => (
                            Outcome::Partial,
                            observed_publication.map(|ledger| ledger.binding.output.clone()),
                            observed_publication.and_then(|ledger| ledger.publication_identity.clone()),
                        ),
                        Some(ItemOutcome::Failed | ItemOutcome::Skipped) if published.is_some() => {
                            let ledger = published.unwrap();
                            (Outcome::Unknown, Some(ledger.binding.output.clone()), ledger.publication_identity.clone())
                        }
                        Some(ItemOutcome::Failed | ItemOutcome::Skipped) if ledger.is_none() || discarded => (Outcome::Failed, None, None),
                        Some(ItemOutcome::CancelledBeforeStart) if published.is_some() => {
                            let ledger = published.unwrap();
                            (Outcome::Unknown, Some(ledger.binding.output.clone()), ledger.publication_identity.clone())
                        }
                        Some(ItemOutcome::CancelledBeforeStart) if ledger.is_none() || discarded => (Outcome::CancelledBeforeStart, None, None),
                        Some(ItemOutcome::Partial | ItemOutcome::Unknown) => (
                            Outcome::Unknown,
                            observed_publication.map(|ledger| ledger.binding.output.clone()),
                            observed_publication.and_then(|ledger| ledger.publication_identity.clone()),
                        ),
                        Some(ItemOutcome::Failed | ItemOutcome::Skipped | ItemOutcome::CancelledBeforeStart) => (
                            Outcome::Unknown,
                            observed_publication.map(|ledger| ledger.binding.output.clone()),
                            observed_publication.and_then(|ledger| ledger.publication_identity.clone()),
                        ),
                        None if matches!(report.outcome, TerminalOutcome::Partial | TerminalOutcome::Unknown) => (
                            Outcome::Unknown,
                            observed_publication.map(|ledger| ledger.binding.output.clone()),
                            observed_publication.and_then(|ledger| ledger.publication_identity.clone()),
                        ),
                        None if published.is_some() => {
                            let ledger = published.unwrap();
                            (Outcome::Unknown, Some(ledger.binding.output.clone()), ledger.publication_identity.clone())
                        }
                        None if ledger.is_none() && report.outcome == TerminalOutcome::Cancelled => (Outcome::CancelledBeforeStart, None, None),
                        None if discarded && report.outcome == TerminalOutcome::Cancelled => (Outcome::CancelledBeforeStart, None, None),
                        None if ledger.is_none() && report.outcome == TerminalOutcome::Failed => (Outcome::Failed, None, None),
                        None if discarded => (Outcome::Failed, None, None),
                        None => (Outcome::Unknown, None, None),
                    };
                    Receipt { ordinal: ordinal as u32, outcome, output, publication_identity }
                }).collect();
                let receipts = &record.seven_zip_extract_receipts;
                let all = |outcome| receipts.iter().all(|receipt| receipt.outcome == outcome);
                record.terminal_outcome =
                    if all(super::journal::JournalSevenZipExtractOutcome::Completed) {
                        Some(super::journal::JournalTerminalOutcome::Succeeded)
                    } else if all(super::journal::JournalSevenZipExtractOutcome::Failed) {
                        Some(super::journal::JournalTerminalOutcome::Failed)
                    } else if all(
                        super::journal::JournalSevenZipExtractOutcome::CancelledBeforeStart,
                    ) {
                        Some(super::journal::JournalTerminalOutcome::Cancelled)
                    } else if receipts.iter().any(|receipt| {
                        receipt.outcome == super::journal::JournalSevenZipExtractOutcome::Unknown
                    }) {
                        Some(super::journal::JournalTerminalOutcome::Unknown)
                    } else {
                        Some(super::journal::JournalTerminalOutcome::Partial)
                    };
                if matches!(
                    record.terminal_outcome,
                    Some(
                        super::journal::JournalTerminalOutcome::Partial
                            | super::journal::JournalTerminalOutcome::Unknown
                    )
                ) {
                    record.state = super::journal::JournalState::RecoveryRequired;
                }
            }
        }
        if (record.replace_backup.is_some()
            || record.rename_stage.is_some()
            || record
                .cross_volume_move_replace
                .as_ref()
                .is_some_and(|ledger| {
                    ledger.phase != super::journal::CrossVolumeMoveReplacePhase::Complete
                }))
            && attempt.terminal.as_ref().is_some_and(|report| {
                matches!(
                    report.outcome,
                    TerminalOutcome::Partial | TerminalOutcome::Unknown
                )
            })
        {
            record.state = super::journal::JournalState::RecoveryRequired;
        }
        record.batch_rename_completed = self
            .batch_rename_completed
            .get(&token)
            .cloned()
            .unwrap_or_default();
        record.copy_replace_cleanups = self
            .copy_replace_cleanups
            .iter()
            .filter(|((attempt_token, _), _)| *attempt_token == token)
            .map(|(_, state)| state.ledger.clone())
            .collect();
        record
            .copy_replace_cleanups
            .sort_by_key(|ledger| ledger.item);
        Some(record)
    }

    pub fn pending_directory_move_manifest(
        &self,
        token: AttemptToken,
        ledger: &super::journal::JournalDirectoryMove,
    ) -> Option<super::journal::JournalAttempt> {
        if self.running != Some(token)
            || !matches!(
                self.directory_moves.get(&(token, super::model::ItemId(ledger.item))).map(|state| &state.barrier),
                Some(DirectoryMoveBarrier::ManifestPending(pending)) if pending == ledger
            )
            || self
                .cancellations
                .get(&token)
                .is_none_or(|flag| flag.load(Ordering::Acquire))
        {
            return None;
        }
        let job = self.job(token)?;
        let source = job.request.sources.get(ledger.item as usize)?;
        let parent = job.request.destination.as_ref()?;
        let base_destination = parent.join(source.file_name().unwrap_or_default());
        let authorized_keep_both = self
            .keep_both_directory_items
            .contains(&(token, super::model::ItemId(ledger.item)));
        let exact_destination = if super::journal::JournalPath::from_path(&base_destination)
            == ledger.destination
        {
            Some(base_destination)
        } else if authorized_keep_both {
            (2..10_000).find_map(|suffix| {
                let candidate = super::model::keep_both_candidate_path(&base_destination, suffix)?;
                (super::journal::JournalPath::from_path(&candidate) == ledger.destination)
                    .then_some(candidate)
            })
        } else {
            None
        };
        if job.request.kind != super::model::OperationKind::Move
            || !job.attempts.iter().any(|attempt| {
                attempt.token == token && attempt.state == super::model::AttemptState::Running
            })
            || super::journal::JournalPath::from_path(source) != ledger.source
            || exact_destination.is_none()
        {
            return None;
        }
        self.journal_attempt(token)
    }

    pub fn acknowledge_directory_move_manifest(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalDirectoryMove,
    ) -> bool {
        if self
            .pending_directory_move_manifest(token, ledger)
            .is_none()
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveManifestJournaledExact {
                token,
                ledger: ledger.clone(),
            })
            .is_err()
        {
            return false;
        }
        if let Some(state) = self
            .directory_moves
            .get_mut(&(token, super::model::ItemId(ledger.item)))
        {
            state.barrier = DirectoryMoveBarrier::Ready;
        }
        if let Some(replace) = ledger.replace.clone() {
            self.directory_move_replaces.insert(
                (token, super::model::ItemId(ledger.item)),
                DirectoryMoveReplaceRuntimeState {
                    phase: replace.phase,
                    ledger: replace,
                    barrier: DirectoryMoveReplaceBarrier::Ready,
                },
            );
        }
        self.keep_both_directory_items
            .remove(&(token, super::model::ItemId(ledger.item)));
        true
    }

    fn directory_move_is_current(&self, token: AttemptToken, item: super::model::ItemId) -> bool {
        self.running == Some(token)
            && !self
                .cancellations
                .get(&token)
                .is_none_or(|flag| flag.load(Ordering::Acquire))
            && self
                .directory_moves
                .get(&(token, item))
                .is_some_and(|state| {
                    state.item == item
                        && self.job(token).is_some_and(|job| {
                            job.request.kind == super::model::OperationKind::Move
                                && job.attempts.iter().any(|attempt| {
                                    attempt.token == token
                                        && attempt.state == super::model::AttemptState::Running
                                })
                                && job
                                    .request
                                    .sources
                                    .get(item.0 as usize)
                                    .is_some_and(|source| {
                                        source == &state.source
                                            && super::journal::JournalPath::from_path(source)
                                                == super::journal::JournalPath::from_path(
                                                    &state.source,
                                                )
                                    })
                                && job.request.destination.as_ref().is_some_and(|parent| {
                                    state.destination.parent() == Some(parent.as_path())
                                        && state.destination.file_name().is_some()
                                })
                        })
                })
    }

    pub fn pending_directory_move_child(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryChildIntent,
    ) -> Option<super::journal::JournalAttempt> {
        (self.directory_move_is_current(token, item)
            && (matches!(self.directory_moves.get(&(token, item))?.barrier,
                    DirectoryMoveBarrier::ChildIntentPending(ref pending) if pending == intent)
                || matches!(self.directory_moves.get(&(token, item))?.barrier,
                    DirectoryMoveBarrier::ChildReceiptPending(ref pending) if pending == intent)))
        .then(|| self.journal_attempt(token))
        .flatten()
    }

    pub fn acknowledge_directory_move_child_intent(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryChildIntent,
    ) -> bool {
        if !self.directory_move_is_current(token, item)
            || !matches!(self.directory_moves.get(&(token, item)).map(|state| &state.barrier),
                Some(DirectoryMoveBarrier::ChildIntentPending(pending)) if pending == intent)
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveChildIntentJournaledExact {
                token,
                item,
                intent: intent.clone(),
            })
            .is_err()
        {
            return false;
        }
        self.directory_moves
            .get_mut(&(token, item))
            .unwrap()
            .barrier = DirectoryMoveBarrier::ChildIntentDurable(intent.clone());
        true
    }

    pub fn acknowledge_directory_move_child_receipt(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryChildIntent,
    ) -> bool {
        if !self.directory_move_is_current(token, item)
            || !matches!(self.directory_moves.get(&(token, item)).map(|state| &state.barrier),
                Some(DirectoryMoveBarrier::ChildReceiptPending(pending)) if pending == intent)
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveChildReceiptJournaledExact {
                token,
                item,
                intent: intent.clone(),
            })
            .is_err()
        {
            return false;
        }
        self.directory_moves
            .get_mut(&(token, item))
            .unwrap()
            .barrier = DirectoryMoveBarrier::Ready;
        true
    }

    pub fn pending_directory_move_root(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryRootIntent,
    ) -> Option<super::journal::JournalAttempt> {
        if !self.directory_move_is_current(token, item) {
            return None;
        }
        (matches!(self.directory_moves.get(&(token, item))?.barrier,
            DirectoryMoveBarrier::RootIntentPending(ref pending) if pending == intent)
            || matches!(self.directory_moves.get(&(token, item))?.barrier,
                DirectoryMoveBarrier::RootReceiptPending(ref pending) if pending == intent))
        .then(|| self.journal_attempt(token))
        .flatten()
    }

    pub fn acknowledge_directory_move_root_intent(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryRootIntent,
    ) -> bool {
        if !self.directory_move_is_current(token, item)
            || !matches!(self.directory_moves.get(&(token, item)).map(|state| &state.barrier),
                Some(DirectoryMoveBarrier::RootIntentPending(pending)) if pending == intent)
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveRootIntentJournaledExact {
                token,
                item,
                intent: intent.clone(),
            })
            .is_err()
        {
            return false;
        }
        self.directory_moves
            .get_mut(&(token, item))
            .unwrap()
            .barrier = DirectoryMoveBarrier::RootIntentDurable(intent.clone());
        true
    }

    pub fn acknowledge_directory_move_root_receipt(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryRootIntent,
    ) -> bool {
        if !self.directory_move_is_current(token, item)
            || !matches!(self.directory_moves.get(&(token, item)).map(|state| &state.barrier),
                Some(DirectoryMoveBarrier::RootReceiptPending(pending)) if pending == intent)
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveRootReceiptJournaledExact {
                token,
                item,
                intent: intent.clone(),
            })
            .is_err()
        {
            return false;
        }
        self.directory_moves
            .get_mut(&(token, item))
            .unwrap()
            .barrier = DirectoryMoveBarrier::Ready;
        true
    }

    pub fn pending_directory_move_complete(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> Option<super::journal::JournalAttempt> {
        (self.directory_move_is_current(token, item)
            && matches!(
                self.directory_moves.get(&(token, item))?.barrier,
                DirectoryMoveBarrier::CompletePending
            ))
        .then(|| self.journal_attempt(token))
        .flatten()
    }

    pub fn acknowledge_directory_move_complete(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> bool {
        if !self.directory_move_is_current(token, item)
            || !matches!(
                self.directory_moves
                    .get(&(token, item))
                    .map(|state| &state.barrier),
                Some(DirectoryMoveBarrier::CompletePending)
            )
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveCompleteJournaledExact { token, item })
            .is_err()
        {
            return false;
        }
        self.directory_moves
            .get_mut(&(token, item))
            .unwrap()
            .barrier = if self.directory_move_replaces.contains_key(&(token, item)) {
            DirectoryMoveBarrier::SourceComplete
        } else {
            DirectoryMoveBarrier::Complete
        };
        if let Some(replace) = self.directory_move_replaces.get_mut(&(token, item)) {
            replace.phase = super::journal::DirectoryMoveReplacePhase::SourceComplete;
            replace.ledger.phase = replace.phase;
            replace.barrier = DirectoryMoveReplaceBarrier::Ready;
        }
        true
    }

    pub fn pending_directory_move_replace_phase(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        phase: super::journal::DirectoryMoveReplacePhase,
    ) -> Option<super::journal::JournalAttempt> {
        let state = self.directory_move_replaces.get(&(token, item))?;
        (self.directory_move_is_current(token, item)
            && state.barrier == DirectoryMoveReplaceBarrier::PhasePending(phase)
            && super::journal::directory_move_replace_phase_next(state.phase, phase))
        .then(|| self.journal_attempt(token))
        .flatten()
    }

    pub fn acknowledge_directory_move_replace_phase(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        phase: super::journal::DirectoryMoveReplacePhase,
    ) -> bool {
        if self
            .pending_directory_move_replace_phase(token, item, phase)
            .is_none()
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveReplacePhaseJournaledExact { token, item, phase })
            .is_err()
        {
            return false;
        }
        let Some(state) = self.directory_move_replaces.get_mut(&(token, item)) else {
            return false;
        };
        state.phase = phase;
        state.ledger.phase = phase;
        state.barrier = DirectoryMoveReplaceBarrier::Ready;
        if phase == super::journal::DirectoryMoveReplacePhase::Complete {
            self.directory_moves
                .get_mut(&(token, item))
                .unwrap()
                .barrier = DirectoryMoveBarrier::Complete;
        }
        true
    }

    pub fn pending_directory_move_replace_backup_transition(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryMoveReplaceChildIntent,
        receipt: bool,
    ) -> Option<super::journal::JournalAttempt> {
        let state = self.directory_move_replaces.get(&(token, item))?;
        let barrier_matches = if receipt {
            state.barrier == DirectoryMoveReplaceBarrier::BackupReceiptPending(intent.clone())
        } else {
            state.barrier == DirectoryMoveReplaceBarrier::BackupIntentPending(intent.clone())
        };
        (barrier_matches
            && self.directory_move_is_current(token, item)
            && state.phase == super::journal::DirectoryMoveReplacePhase::BackupCleanup)
            .then(|| self.journal_attempt(token))
            .flatten()
    }

    pub fn acknowledge_directory_move_replace_backup_intent(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryMoveReplaceChildIntent,
    ) -> bool {
        if !self.directory_move_is_current(token, item) {
            return false;
        }
        let Some(state) = self.directory_move_replaces.get(&(token, item)) else {
            return false;
        };
        if state.barrier != DirectoryMoveReplaceBarrier::BackupIntentPending(intent.clone())
            || state.phase != super::journal::DirectoryMoveReplacePhase::BackupCleanup
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(
                WorkerControl::DirectoryMoveReplaceBackupIntentJournaledExact {
                    token,
                    item,
                    intent: intent.clone(),
                },
            )
            .is_err()
        {
            return false;
        }
        self.directory_move_replaces
            .get_mut(&(token, item))
            .unwrap()
            .barrier = DirectoryMoveReplaceBarrier::BackupIntentDurable(intent.clone());
        true
    }

    pub fn acknowledge_directory_move_replace_backup_receipt(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalDirectoryMoveReplaceChildIntent,
    ) -> bool {
        if !self.directory_move_is_current(token, item) {
            return false;
        }
        let Some(state) = self.directory_move_replaces.get(&(token, item)) else {
            return false;
        };
        if state.barrier != DirectoryMoveReplaceBarrier::BackupReceiptPending(intent.clone())
            || state.phase != super::journal::DirectoryMoveReplacePhase::BackupCleanup
        {
            return false;
        }
        let is_root = intent.ordinal as usize == state.ledger.backup_children.len();
        if is_root {
            if !state
                .ledger
                .backup_children
                .iter()
                .all(|child| child.disposition_recorded)
            {
                return false;
            }
            // Root receipt is applied after the exact control is queued.
        } else {
            let Some(child) = state.ledger.backup_children.get(intent.ordinal as usize) else {
                return false;
            };
            if child.disposition_recorded
                || child.node_kind != intent.node_kind
                || child.relative_components != intent.relative_components
                || child.identity != intent.identity
                || child.content_fact != intent.content_fact
            {
                return false;
            }
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(
                WorkerControl::DirectoryMoveReplaceBackupReceiptJournaledExact {
                    token,
                    item,
                    intent: intent.clone(),
                },
            )
            .is_err()
        {
            return false;
        }
        let state = self
            .directory_move_replaces
            .get_mut(&(token, item))
            .unwrap();
        if is_root {
            state.ledger.backup_root_disposition_recorded = true;
        } else {
            state.ledger.backup_children[intent.ordinal as usize].disposition_recorded = true;
        }
        state.ledger.backup_pending = None;
        state.barrier = DirectoryMoveReplaceBarrier::Ready;
        true
    }

    pub fn directory_move_replace_backup_complete_pending(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> bool {
        self.directory_move_replaces
            .get(&(token, item))
            .is_some_and(|state| {
                self.directory_move_is_current(token, item)
                    && state.barrier == DirectoryMoveReplaceBarrier::BackupCompletePending
            })
    }

    pub fn acknowledge_directory_move_replace_backup_complete(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> bool {
        if !self.directory_move_replace_backup_complete_pending(token, item) {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::DirectoryMoveReplaceBackupCompleteJournaledExact { token, item })
            .is_err()
        {
            return false;
        }
        let Some(state) = self.directory_move_replaces.get_mut(&(token, item)) else {
            return false;
        };
        state.phase = super::journal::DirectoryMoveReplacePhase::BackupComplete;
        state.ledger.phase = state.phase;
        state.ledger.backup_cleanup_complete = true;
        state.barrier = DirectoryMoveReplaceBarrier::BackupComplete;
        true
    }

    pub fn directory_move_terminal_candidate(
        &mut self,
        token: AttemptToken,
        report: &super::model::TerminalReport,
    ) -> Option<super::journal::JournalAttempt> {
        if report.outcome != TerminalOutcome::Succeeded
            || self.running != Some(token)
            || self
                .cancellations
                .get(&token)
                .is_none_or(|flag| flag.load(Ordering::Acquire))
            || !self
                .directory_moves
                .iter()
                .filter(|((attempt, _), _)| *attempt == token)
                .all(|((attempt, item), state)| {
                    *attempt == token
                        && matches!(state.barrier, DirectoryMoveBarrier::Complete)
                        && self.directory_move_is_current(*attempt, *item)
                })
            || !self
                .directory_moves
                .keys()
                .any(|(attempt, _)| *attempt == token)
        {
            return None;
        }
        let mut record = self.journal_attempt(token)?;
        for ((attempt, _), state) in self.directory_moves.iter_mut() {
            if *attempt == token {
                state.barrier = DirectoryMoveBarrier::TerminalPending;
            }
        }
        record.state = super::journal::JournalState::Terminal;
        record.receipts_recorded = true;
        record.terminal_outcome = Some(super::journal::JournalTerminalOutcome::Succeeded);
        Some(record)
    }

    pub fn commit_directory_move_terminal(
        &mut self,
        token: AttemptToken,
        mut report: super::model::TerminalReport,
        terminal_record_durable: bool,
    ) -> Option<TerminalCommit> {
        if self.running != Some(token) {
            return None;
        }
        let has_directory_moves = self
            .directory_moves
            .keys()
            .any(|(attempt, _)| *attempt == token);
        let all_directory_moves_terminal_pending = self
            .directory_moves
            .iter()
            .filter(|((attempt, _), _)| *attempt == token)
            .all(|((attempt, item), state)| {
                *attempt == token
                    && matches!(state.barrier, DirectoryMoveBarrier::TerminalPending)
                    && self.directory_move_is_current(*attempt, *item)
            });
        if has_directory_moves
            && (!terminal_record_durable
                || self
                    .cancellations
                    .get(&token)
                    .is_none_or(|flag| flag.load(Ordering::Acquire))
                || !all_directory_moves_terminal_pending)
        {
            report.outcome = TerminalOutcome::Partial;
            report.retry = super::model::RetryPlan::RequiresReinspection;
        }
        self.service
            .terminal(token, report)
            .map(|event| self.accept_terminal(event))
    }

    pub fn acknowledge_replace_backup(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        source: &std::path::Path,
        destination: &std::path::Path,
        backup: &std::path::Path,
    ) -> bool {
        if !self.is_current_replace_backup_intent(token, item, source, destination, backup) {
            return false;
        }
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::ReplaceBackupJournaledExact {
                    token,
                    item,
                    source: source.to_path_buf(),
                    destination: destination.to_path_buf(),
                    backup: backup.to_path_buf(),
                })
                .is_ok()
        })
    }

    pub fn acknowledge_cross_volume_move_replace_phase(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalCrossVolumeMoveReplace,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_cross_volume_move_replace.get(&token) != Some(ledger)
        {
            return false;
        }
        let sent = self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::CrossVolumeMoveReplacePhaseJournaledExact {
                    token,
                    ledger: ledger.clone(),
                })
                .is_ok()
        });
        if sent {
            if ledger.phase == super::journal::CrossVolumeMoveReplacePhase::StageCreateIntent {
                self.replace_backups.insert(
                    token,
                    super::journal::JournalReplaceBackup {
                        source: ledger.source.clone(),
                        destination: ledger.destination.clone(),
                        backup: ledger.backup.clone(),
                    },
                );
            }
            self.cross_volume_move_replaces
                .insert(token, ledger.clone());
            self.pending_cross_volume_move_replace.remove(&token);
        }
        sent
    }

    pub fn acknowledge_directory_move_replace_rename_phase(
        &mut self,
        token: AttemptToken,
        conflict_token: super::model::ConflictToken,
        ledger: &super::journal::JournalDirectoryMoveReplaceRename,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_directory_move_replace_rename.get(&token)
                != Some(&(conflict_token, ledger.clone()))
            || self.service.accepted_directory_move_replace_rename(conflict_token)
                .is_none_or(|candidate| !directory_move_replace_rename_matches_candidate(&candidate, ledger))
        {
            return false;
        }
        let sent = self.controls.get(&token).is_some_and(|control| {
            control.send(WorkerControl::DirectoryMoveReplaceRenamePhaseJournaledExact {
                token,
                conflict_token,
                ledger: ledger.clone(),
            }).is_ok()
        });
        if sent {
            self.directory_move_replace_rename.insert(token, ledger.clone());
            self.pending_directory_move_replace_rename.remove(&token);
            if ledger.phase == super::journal::DirectoryMoveReplaceRenamePhase::BackupCleanup {
                self.pending_directory_move_replace_rename_cleanup.entry(token).or_insert((ledger.approved_target_children.first().map(|child| super::journal::JournalDirectoryMoveReplaceRenameChildIntent { ordinal: child.ordinal, node_kind: child.node_kind, relative_components: child.relative_components.clone(), identity: child.identity.clone(), content_fact: child.content_fact.clone() }).unwrap_or(super::journal::JournalDirectoryMoveReplaceRenameChildIntent { ordinal: 0, node_kind: super::journal::JournalDirectoryNodeKind::Directory, relative_components: Vec::new(), identity: ledger.approved_target_identity.clone(), content_fact: "root".into() }), RenameCleanupBarrier::Ready));
            }
        }
        sent
    }

    pub fn directory_move_replace_rename_phase_pending(
        &self,
        token: AttemptToken,
        conflict_token: super::model::ConflictToken,
        ledger: &super::journal::JournalDirectoryMoveReplaceRename,
    ) -> bool {
        self.running == Some(token)
            && self.pending_directory_move_replace_rename.get(&token)
                == Some(&(conflict_token, ledger.clone()))
    }

    pub fn reject_directory_move_replace_rename_phase(
        &mut self,
        token: AttemptToken,
        conflict_token: super::model::ConflictToken,
        ledger: &super::journal::JournalDirectoryMoveReplaceRename,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_directory_move_replace_rename.get(&token)
                != Some(&(conflict_token, ledger.clone()))
        {
            return false;
        }
        let sent = self.controls.get(&token).is_some_and(|control| {
            control.send(WorkerControl::DirectoryMoveReplaceRenamePhaseJournalFailedExact {
                    token,
                    conflict_token,
                    ledger: ledger.clone(),
                }).is_ok()
        });
        if sent {
            self.pending_directory_move_replace_rename.remove(&token);
        }
        sent
    }

    pub fn pending_directory_move_replace_rename_cleanup_transition(
        &self, token: AttemptToken, intent: &super::journal::JournalDirectoryMoveReplaceRenameChildIntent, receipt: bool,
    ) -> bool {
        self.running == Some(token)
            && self.pending_directory_move_replace_rename_cleanup.get(&token).is_some_and(|(current, barrier)| {
            current == intent && *barrier == if receipt { RenameCleanupBarrier::ReceiptPending } else { RenameCleanupBarrier::IntentPending }
        })
    }

    pub fn acknowledge_directory_move_replace_rename_cleanup_intent(
        &mut self, token: AttemptToken, intent: &super::journal::JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> bool {
        if self.running != Some(token) || !self.pending_directory_move_replace_rename_cleanup_transition(token, intent, false) { return false; }
        let Some(control) = self.controls.get(&token) else { return false; };
        if control.send(WorkerControl::DirectoryMoveReplaceRenameCleanupIntentJournaledExact { token, item: super::model::ItemId(0), intent: intent.clone() }).is_err() { return false; }
        if let Some(ledger) = self.directory_move_replace_rename.get_mut(&token) { ledger.pending_cleanup = Some(intent.clone()); }
        if let Some((_, barrier)) = self.pending_directory_move_replace_rename_cleanup.get_mut(&token) { *barrier = RenameCleanupBarrier::IntentDurable; }
        true
    }

    pub fn reject_directory_move_replace_rename_cleanup_intent(
        &self, token: AttemptToken, intent: &super::journal::JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> bool {
        self.running == Some(token) && self.pending_directory_move_replace_rename_cleanup_transition(token, intent, false)
            && self.controls.get(&token).is_some_and(|control| control.send(WorkerControl::Cancel).is_ok())
    }

    pub fn acknowledge_directory_move_replace_rename_cleanup_receipt(
        &mut self, token: AttemptToken, intent: &super::journal::JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> bool {
        if self.running != Some(token) || !self.pending_directory_move_replace_rename_cleanup_transition(token, intent, true) { return false; }
        let Some(control) = self.controls.get(&token) else { return false; };
        if control.send(WorkerControl::DirectoryMoveReplaceRenameCleanupReceiptJournaledExact { token, item: super::model::ItemId(0), intent: intent.clone() }).is_err() { return false; }
        if let Some(ledger) = self.directory_move_replace_rename.get_mut(&token) {
            if intent.ordinal as usize == ledger.approved_target_children.len() {
                ledger.backup_root_disposed = true;
                ledger.target_location = super::journal::DirectoryMoveReplaceTargetLocation::Disposed;
            } else if let Some(child) = ledger.approved_target_children.get_mut(intent.ordinal as usize) { child.disposition_recorded = true; }
            ledger.pending_cleanup = None;
        }
        if let Some((_, barrier)) = self.pending_directory_move_replace_rename_cleanup.get_mut(&token) { *barrier = RenameCleanupBarrier::Ready; }
        true
    }

    pub fn reject_directory_move_replace_rename_cleanup_receipt(&self, token: AttemptToken, intent: &super::journal::JournalDirectoryMoveReplaceRenameChildIntent) -> bool {
        self.running == Some(token) && self.pending_directory_move_replace_rename_cleanup_transition(token, intent, true)
            && self.controls.get(&token).is_some_and(|control| control.send(WorkerControl::Cancel).is_ok())
    }

    pub fn directory_move_replace_rename_backup_complete_pending(&self, token: AttemptToken) -> bool {
        self.running == Some(token)
            && self.pending_directory_move_replace_rename_cleanup.get(&token).is_some_and(|(_, barrier)| *barrier == RenameCleanupBarrier::BackupCompletePending)
    }

    pub fn acknowledge_directory_move_replace_rename_backup_complete(&mut self, token: AttemptToken) -> bool {
        if self.running != Some(token) || !self.directory_move_replace_rename_backup_complete_pending(token) { return false; }
        let Some(control) = self.controls.get(&token) else { return false; };
        if control.send(WorkerControl::DirectoryMoveReplaceRenameBackupCompleteJournaledExact { token, item: super::model::ItemId(0) }).is_err() { return false; }
        if let Some(ledger) = self.directory_move_replace_rename.get_mut(&token) { ledger.phase = super::journal::DirectoryMoveReplaceRenamePhase::BackupComplete; }
        if let Some((_, barrier)) = self.pending_directory_move_replace_rename_cleanup.get_mut(&token) { *barrier = RenameCleanupBarrier::Ready; }
        true
    }

    pub fn reject_directory_move_replace_rename_backup_complete(&self, token: AttemptToken) -> bool {
        self.running == Some(token) && self.directory_move_replace_rename_backup_complete_pending(token)
            && self.controls.get(&token).is_some_and(|control| control.send(WorkerControl::Cancel).is_ok())
    }

    pub fn reject_cross_volume_move_replace_phase(
        &self,
        token: AttemptToken,
        ledger: &super::journal::JournalCrossVolumeMoveReplace,
    ) -> bool {
        self.running == Some(token)
            && self.pending_cross_volume_move_replace.get(&token) == Some(ledger)
            && self.controls.get(&token).is_some_and(|control| {
                control
                    .send(
                        WorkerControl::CrossVolumeMoveReplacePhaseJournalFailedExact {
                            token,
                            ledger: ledger.clone(),
                        },
                    )
                    .is_ok()
            })
    }

    /// Releases a current worker waiting on a phase fact that did not match
    /// the Runtime-owned pending phase. The rejected fact is never adopted.
    pub fn reject_unaccepted_cross_volume_move_replace_phase(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalCrossVolumeMoveReplace,
    ) -> bool {
        if self.running != Some(token) {
            return false;
        }
        let Some(cancelled) = self.cancellations.get(&token) else {
            return false;
        };
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::CrossVolumeMoveReplacePhaseJournalFailedExact {
                token,
                ledger: ledger.clone(),
            })
            .is_err()
        {
            return false;
        }
        cancelled.store(true, Ordering::Release);
        true
    }

    pub fn is_current_cross_volume_move_replace_phase(
        &self,
        token: AttemptToken,
        ledger: &super::journal::JournalCrossVolumeMoveReplace,
    ) -> bool {
        self.running == Some(token)
            && self.pending_cross_volume_move_replace.get(&token) == Some(ledger)
    }

    pub fn pending_copy_replace_cleanup_manifest(
        &self,
        token: AttemptToken,
        ledger: &super::journal::JournalCopyReplaceCleanup,
    ) -> bool {
        self.copy_replace_cleanups
            .get(&(token, super::model::ItemId(ledger.item)))
            .is_some_and(|state| {
                state.ledger == *ledger
                    && state.barrier == CopyReplaceCleanupBarrier::ManifestPending
            })
    }

    pub fn acknowledge_copy_replace_cleanup_manifest(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalCopyReplaceCleanup,
    ) -> bool {
        let key = (token, super::model::ItemId(ledger.item));
        if !self.copy_replace_cleanup_current(token, key.1) {
            return false;
        }
        let Some(state) = self.copy_replace_cleanups.get_mut(&key) else {
            return false;
        };
        if state.ledger != *ledger || state.barrier != CopyReplaceCleanupBarrier::ManifestPending {
            return false;
        }
        state.barrier = CopyReplaceCleanupBarrier::Ready;
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::CopyReplaceCleanupManifestJournaledExact {
                    token,
                    ledger: ledger.clone(),
                })
                .is_ok()
        })
    }

    pub fn pending_copy_replace_cleanup_transition(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalCopyReplaceChildIntent,
        receipt: bool,
    ) -> bool {
        let Some(state) = self.copy_replace_cleanups.get(&(token, item)) else {
            return false;
        };
        if receipt {
            state.barrier == CopyReplaceCleanupBarrier::ReceiptPending(intent.clone())
        } else {
            state.barrier == CopyReplaceCleanupBarrier::IntentPending(intent.clone())
        }
    }

    pub fn copy_replace_cleanup_complete_pending(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> bool {
        self.copy_replace_cleanups
            .get(&(token, item))
            .is_some_and(|state| state.barrier == CopyReplaceCleanupBarrier::CompletePending)
    }

    pub fn acknowledge_copy_replace_cleanup_intent(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalCopyReplaceChildIntent,
    ) -> bool {
        if !self.copy_replace_cleanup_current(token, item) {
            return false;
        }
        let Some(state) = self.copy_replace_cleanups.get_mut(&(token, item)) else {
            return false;
        };
        if state.barrier != CopyReplaceCleanupBarrier::IntentPending(intent.clone()) {
            return false;
        }
        state.barrier = CopyReplaceCleanupBarrier::IntentDurable(intent.clone());
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::CopyReplaceCleanupChildIntentJournaledExact {
                    token,
                    item,
                    intent: intent.clone(),
                })
                .is_ok()
        })
    }

    pub fn acknowledge_copy_replace_cleanup_receipt(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        intent: &super::journal::JournalCopyReplaceChildIntent,
    ) -> bool {
        if self.running != Some(token) {
            return false;
        }
        let Some(state) = self.copy_replace_cleanups.get_mut(&(token, item)) else {
            return false;
        };
        if state.barrier != CopyReplaceCleanupBarrier::ReceiptPending(intent.clone()) {
            return false;
        }
        let root = intent.ordinal as usize == state.ledger.children.len();
        if root {
            state.ledger.root_disposition_recorded = true;
        } else if let Some(child) = state.ledger.children.get_mut(intent.ordinal as usize) {
            child.disposition_recorded = true;
        } else {
            return false;
        }
        state.ledger.pending = None;
        state.barrier = CopyReplaceCleanupBarrier::Ready;
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(
                    WorkerControl::CopyReplaceCleanupChildReceiptJournaledExact {
                        token,
                        item,
                        intent: intent.clone(),
                    },
                )
                .is_ok()
        })
    }

    pub fn acknowledge_copy_replace_cleanup_complete(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> bool {
        if self.running != Some(token) {
            return false;
        }
        let Some(state) = self.copy_replace_cleanups.get_mut(&(token, item)) else {
            return false;
        };
        if state.barrier != CopyReplaceCleanupBarrier::CompletePending {
            return false;
        }
        state.ledger.cleanup_complete = true;
        state.barrier = CopyReplaceCleanupBarrier::Complete;
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::CopyReplaceCleanupCompleteJournaledExact { token, item })
                .is_ok()
        })
    }

    fn copy_replace_cleanup_current(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> bool {
        self.running == Some(token)
            && self
                .cancellations
                .get(&token)
                .is_some_and(|flag| !flag.load(Ordering::Acquire))
            && self.copy_replace_cleanups.contains_key(&(token, item))
    }

    pub fn is_current_replace_backup_intent(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        source: &std::path::Path,
        destination: &std::path::Path,
        backup: &std::path::Path,
    ) -> bool {
        self.running == Some(token)
            && self
                .cancellations
                .get(&token)
                .is_some_and(|flag| !flag.load(Ordering::Acquire))
            && self
                .pending_replace_intents
                .get(&token)
                .is_some_and(|intent| {
                    intent
                        == &(
                            item,
                            source.to_path_buf(),
                            destination.to_path_buf(),
                            backup.to_path_buf(),
                        )
                })
    }

    pub fn acknowledge_rename_stage_intent(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
        source: &std::path::Path,
        stage: &std::path::Path,
        destination: &std::path::Path,
        source_identity: &str,
        parent_identity: &str,
        phase: u8,
    ) -> bool {
        let exact = (
            item,
            source.to_path_buf(),
            stage.to_path_buf(),
            destination.to_path_buf(),
            source_identity.to_owned(),
            parent_identity.to_owned(),
            phase,
        );
        if self.running != Some(token)
            || self.pending_rename_intents.get(&token) != Some(&exact)
            || self
                .cancellations
                .get(&token)
                .is_none_or(|flag| flag.load(Ordering::Acquire))
        {
            return false;
        }
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::RenameStageJournaledExact {
                    token,
                    item,
                    source: source.to_path_buf(),
                    stage: stage.to_path_buf(),
                    destination: destination.to_path_buf(),
                    source_identity: source_identity.to_owned(),
                    parent_identity: parent_identity.to_owned(),
                    phase,
                })
                .is_ok()
        })
    }

    pub fn acknowledge_zip_create_ledger(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalZipCreate,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_zip_create_intents.get(&token) != Some(ledger)
            || (self
                .cancellations
                .get(&token)
                .is_none_or(|flag| flag.load(Ordering::Acquire))
                && ledger.phase != super::journal::JournalZipCreatePhase::StageDiscarded)
        {
            return false;
        }
        let acknowledged = self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::ZipCreateLedgerJournaledExact {
                    token,
                    ledger: ledger.clone(),
                })
                .is_ok()
        });
        if acknowledged {
            self.pending_zip_create_intents.remove(&token);
        }
        acknowledged
    }

    pub fn reject_zip_create_ledger(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalZipCreate,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_zip_create_intents.get(&token) != Some(ledger)
        {
            return false;
        }
        let delivered = self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::ZipCreateLedgerJournalFailedExact {
                    token,
                    ledger: ledger.clone(),
                })
                .is_ok()
        });
        self.pending_zip_create_intents.remove(&token);
        delivered
    }

    pub fn acknowledge_seven_zip_create_ledger(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalSevenZipHandleCreate,
    ) -> bool {
        let from_pending = self.pending_seven_zip_create_intents.get(&token) == Some(ledger);
        let from_failed = self.failed_seven_zip_create_journal.get(&token) == Some(ledger);
        if (self.running != Some(token)
            && self.job(token).is_none_or(|job| {
                !job.attempts.iter().any(|attempt| {
                    attempt.token == token
                        && matches!(attempt.state, super::model::AttemptState::Terminal)
                })
            }))
            || (!from_pending && !from_failed)
            || ledger.validate().is_err()
        {
            return false;
        }
        let terminal = self.job(token).is_some_and(|job| {
            job.attempts.iter().any(|attempt| {
                attempt.token == token
                    && matches!(attempt.state, super::model::AttemptState::Terminal)
            })
        });
        let cancelled = self
            .cancellations
            .get(&token)
            .is_some_and(|flag| flag.load(Ordering::Acquire));
        let failed_latch = self.seven_zip_create_journal_failed.contains(&token);
        let cleanup_target = if from_pending && !from_failed && !terminal && !failed_latch {
            self.seven_zip_creates.get(&token).and_then(|previous| {
                if ledger.candidate.state
                    == super::journal::JournalSevenZipHandleObjectState::DeleteIntent
                    && previous.candidate.state
                        == super::journal::JournalSevenZipHandleObjectState::Owned
                    && ledger.stage == previous.stage
                {
                    Some(super::executor::SevenZipCreateCleanupTarget::Candidate)
                } else if ledger.stage.state
                    == super::journal::JournalSevenZipHandleObjectState::DeleteIntent
                    && previous.stage.state
                        == super::journal::JournalSevenZipHandleObjectState::Owned
                    && ledger.candidate == previous.candidate
                {
                    Some(super::executor::SevenZipCreateCleanupTarget::Stage)
                } else {
                    None
                }
            })
        } else {
            None
        };
        let acknowledged = self.controls.get(&token).is_some_and(|control| {
            let message = if let Some(target) = cleanup_target {
                WorkerControl::SevenZipCreateCleanupJournaledExact {
                    token,
                    ledger: ledger.clone(),
                    target,
                }
            } else if cancelled || terminal || from_failed || failed_latch {
                WorkerControl::SevenZipCreateLedgerJournaledCancelledExact {
                    token,
                    ledger: ledger.clone(),
                }
            } else {
                WorkerControl::SevenZipCreateLedgerJournaledExact {
                    token,
                    ledger: ledger.clone(),
                }
            };
            control.send(message).is_ok()
        });
        // Disk success is durable even if the worker channel has already
        // closed; that case must not authorize any subsequent mutation.
        self.pending_seven_zip_create_intents.remove(&token);
        self.failed_seven_zip_create_journal.remove(&token);
        self.seven_zip_creates.insert(token, ledger.clone());
        acknowledged || terminal || self.controls.get(&token).is_none()
    }

    pub fn reject_seven_zip_create_ledger(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalSevenZipHandleCreate,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_seven_zip_create_intents.get(&token) != Some(ledger)
        {
            return false;
        }
        let delivered = self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::SevenZipCreateLedgerJournalFailedExact {
                    token,
                    ledger: ledger.clone(),
                })
                .is_ok()
        });
        if delivered {
            self.pending_seven_zip_create_intents.remove(&token);
            self.failed_seven_zip_create_journal
                .insert(token, ledger.clone());
            self.seven_zip_create_journal_failed.insert(token);
        }
        delivered
    }

    pub fn acknowledge_zip_extract_ledger(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalZipExtract,
    ) -> bool {
        self.acknowledge_zip_extract_item_ledger(token, super::model::ItemId(0), ledger)
    }

    pub fn acknowledge_zip_extract_item_ledger(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        ledger: &super::journal::JournalZipExtract,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_zip_extract_intents.get(&(token, item)) != Some(ledger)
            || (self
                .cancellations
                .get(&token)
                .is_none_or(|flag| flag.load(Ordering::Acquire))
                && !matches!(
                    ledger.phase,
                    super::journal::JournalZipExtractPhase::StagePlanned
                        | super::journal::JournalZipExtractPhase::StageDiscarded
                ))
        {
            return false;
        }
        let acknowledged = self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::ZipExtractLedgerJournaledExact {
                    token,
                    item,
                    ledger: ledger.clone(),
                })
                .is_ok()
        });
        if acknowledged {
            self.pending_zip_extract_intents.remove(&(token, item));
        }
        acknowledged
    }

    pub fn claim_pending_seven_zip_extract_batch_write(
        &mut self,
        token: AttemptToken,
        batch: &super::journal::JournalSevenZipExtractBatch,
    ) -> Option<super::journal::JournalAttempt> {
        let state = self.seven_zip_extract_batches.get_mut(&token)?;
        if state.pending.as_ref() != Some(batch) || state.pending_write_claimed {
            return None;
        }
        state.pending_write_claimed = true;
        state.pending_record.clone()
    }

    pub fn seven_zip_extract_repair_records(
        &mut self,
        token: AttemptToken,
    ) -> Option<(
        super::journal::JournalSevenZipExtractBatch,
        super::journal::JournalAttempt,
        Option<(
            super::journal::JournalSevenZipExtractBatch,
            super::journal::JournalAttempt,
        )>,
    )> {
        let state = self.seven_zip_extract_batches.get_mut(&token)?;
        if state.cancel_requested {
            promote_extract_retry_record(state.failed_record.as_mut());
            promote_extract_retry_record(state.pending_record.as_mut());
        }
        let failed = state.failed.as_ref()?.clone();
        let failed_record = state.failed_record.as_ref()?.clone();
        let pending = state
            .pending
            .as_ref()
            .zip(state.pending_record.as_ref())
            .map(|(batch, record)| (batch.clone(), record.clone()));
        Some((failed, failed_record, pending))
    }

    pub fn is_seven_zip_extract_attempt(&self, token: AttemptToken) -> bool {
        self.job(token)
            .is_some_and(|job| job.request.kind == super::model::OperationKind::Extract7z)
    }

    pub fn frozen_seven_zip_extract_terminal_record(
        &self,
        token: AttemptToken,
    ) -> Option<super::journal::JournalAttempt> {
        self.seven_zip_extract_batches
            .get(&token)?
            .frozen_terminal_record
            .clone()
    }

    pub fn seven_zip_extract_terminal_write_plan(
        &mut self,
        token: AttemptToken,
    ) -> Option<(
        Vec<(
            super::journal::JournalSevenZipExtractBatch,
            super::journal::JournalAttempt,
        )>,
        super::journal::JournalAttempt,
    )> {
        let state = self.seven_zip_extract_batches.get_mut(&token)?;
        if state.cancel_requested {
            promote_extract_retry_record(state.failed_record.as_mut());
            promote_extract_retry_record(state.pending_record.as_mut());
        }
        let terminal = state.frozen_terminal_record.clone()?;
        let mut snapshots = Vec::with_capacity(2);
        if let Some(batch) = &state.failed {
            snapshots.push((batch.clone(), state.failed_record.clone()?));
        }
        if let Some(batch) = &state.pending {
            snapshots.push((batch.clone(), state.pending_record.clone()?));
        }
        Some((snapshots, terminal))
    }

    pub fn acknowledge_seven_zip_extract_repair_batch(
        &mut self,
        token: AttemptToken,
        batch: &super::journal::JournalSevenZipExtractBatch,
    ) -> bool {
        let Some(state) = self.seven_zip_extract_batches.get_mut(&token) else {
            return false;
        };
        if state.pending.as_ref() == Some(batch) && state.failed.is_some() {
            return false;
        }
        if state.failed.as_ref() == Some(batch) {
            state.durable = Some(batch.clone());
            state.failed = None;
            state.failed_record = None;
        } else if state.pending.as_ref() == Some(batch) && state.failed.is_none() {
            state.durable = Some(batch.clone());
            state.pending = None;
            state.pending_record = None;
            state.pending_write_claimed = false;
        } else {
            return false;
        }
        if self.running == Some(token)
            && let Some(control) = self.controls.get(&token)
        {
            let _ = control.send(WorkerControl::SevenZipExtractBatchObservedExact {
                token,
                batch: batch.clone(),
            });
        }
        true
    }

    pub fn reject_seven_zip_extract_repair_batch(
        &mut self,
        token: AttemptToken,
        batch: &super::journal::JournalSevenZipExtractBatch,
    ) -> bool {
        let Some(state) = self.seven_zip_extract_batches.get_mut(&token) else {
            return false;
        };
        if state.failed.as_ref() == Some(batch) {
            state.journal_failed = true;
        } else if state.pending.as_ref() == Some(batch) {
            if state.failed.is_none() {
                state.failed = state.pending.take();
                state.failed_record = state.pending_record.take();
                state.pending_write_claimed = false;
            }
            state.journal_failed = true;
        } else {
            return false;
        }
        if self.running == Some(token)
            && let Some(control) = self.controls.get(&token)
        {
            let _ = control.send(WorkerControl::SevenZipExtractBatchJournalFailedExact {
                token,
                batch: batch.clone(),
            });
        }
        true
    }

    pub fn acknowledge_seven_zip_extract_batch(
        &mut self,
        token: AttemptToken,
        batch: &super::journal::JournalSevenZipExtractBatch,
    ) -> bool {
        let Some(state) = self.seven_zip_extract_batches.get_mut(&token) else {
            return false;
        };
        if state.pending.as_ref() == Some(batch) && state.failed.is_some() {
            return false;
        }
        if state.pending.as_ref() == Some(batch) {
            state.pending = None;
            state.pending_record = None;
            state.pending_write_claimed = false;
        } else if state.journal_failed && state.failed.as_ref() == Some(batch) {
            state.durable = Some(batch.clone());
            if self.running == Some(token)
                && let Some(control) = self.controls.get(&token)
            {
                let _ = control.send(WorkerControl::SevenZipExtractBatchObservedExact {
                    token,
                    batch: batch.clone(),
                });
            }
            return true;
        } else {
            return false;
        }
        let was_failed = state.journal_failed;
        let cleanup = seven_zip_extract_cleanup_transition(state.durable.as_ref(), batch);
        state.durable = Some(batch.clone());
        if let Some(control) = self.controls.get(&token) {
            let cancelled = self
                .cancellations
                .get(&token)
                .is_some_and(|flag| flag.load(Ordering::Acquire));
            if was_failed && self.running == Some(token) {
                let _ = control.send(WorkerControl::SevenZipExtractBatchObservedExact {
                    token,
                    batch: batch.clone(),
                });
                return true;
            } else if self.running != Some(token) {
                return true;
            }
            if let Some((item, target)) = cleanup {
                let _ = control.send(WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                    token,
                    batch: batch.clone(),
                    item,
                    target,
                });
            } else if cancelled {
                let _ = control.send(WorkerControl::SevenZipExtractBatchObservedExact {
                    token,
                    batch: batch.clone(),
                });
            } else {
                let _ = control.send(WorkerControl::SevenZipExtractBatchJournaledExact {
                    token,
                    batch: batch.clone(),
                });
            }
        }
        true
    }

    pub fn reject_seven_zip_extract_batch(
        &mut self,
        token: AttemptToken,
        batch: &super::journal::JournalSevenZipExtractBatch,
    ) -> bool {
        let Some(state) = self.seven_zip_extract_batches.get_mut(&token) else {
            return false;
        };
        if state.pending.as_ref() != Some(batch) {
            return false;
        }
        state.pending = None;
        state.failed = Some(batch.clone());
        state.failed_record = state.pending_record.take();
        state.pending_write_claimed = false;
        state.journal_failed = true;
        if self.running == Some(token)
            && let Some(control) = self.controls.get(&token)
        {
            let _ = control.send(WorkerControl::SevenZipExtractBatchJournalFailedExact {
                token,
                batch: batch.clone(),
            });
        }
        true
    }

    pub fn reject_zip_extract_ledger(
        &mut self,
        token: AttemptToken,
        ledger: &super::journal::JournalZipExtract,
    ) -> bool {
        self.reject_zip_extract_item_ledger(token, super::model::ItemId(0), ledger)
    }

    pub fn reject_zip_extract_item_ledger(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        ledger: &super::journal::JournalZipExtract,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_zip_extract_intents.get(&(token, item)) != Some(ledger)
        {
            return false;
        }
        let delivered = self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::ZipExtractLedgerJournalFailedExact {
                    token,
                    item,
                    ledger: ledger.clone(),
                })
                .is_ok()
        });
        self.pending_zip_extract_intents.remove(&(token, item));
        delivered
    }

    pub fn submit(&mut self, request: OperationRequest) -> Result<AttemptToken, SubmitError> {
        if self.run_id.is_none() {
            return Err(SubmitError::IdentityUnavailable);
        }
        if request.kind == super::model::OperationKind::CopyStageCleanup {
            return Err(SubmitError::InvalidRequest);
        }
        let shortcut_create_shape_is_valid = request.shortcut_create_shape_is_valid();
        if (request.kind == super::model::OperationKind::CreateShortcut
            && !shortcut_create_shape_is_valid)
            || (request.kind != super::model::OperationKind::CreateShortcut
                && request.shortcut_create_guard.is_some())
        {
            return Err(SubmitError::InvalidRequest);
        }
        let invalid_virtual_sources = request.virtual_source.as_ref().is_some_and(|sources| {
            sources.items.is_empty() || sources.items.len() > 1024 || {
                let mut tokens = std::collections::HashSet::with_capacity(sources.items.len());
                sources
                    .items
                    .iter()
                    .any(|source| !tokens.insert(source.token))
            }
        });
        if request.virtual_source.is_some()
            && (invalid_virtual_sources
                || request.kind != super::model::OperationKind::Copy
                || !request.sources.is_empty()
                || request.destination.is_none()
                || request.batch_rename_plan.is_some())
        {
            return Err(SubmitError::InvalidRequest);
        }
        let create_parent_shape_is_valid = request.sources.is_empty()
            && matches!(
                request.kind,
                super::model::OperationKind::CreateFolder | super::model::OperationKind::CreateFile
            )
            && request.destination.is_some()
            && request.create_parent_directory_identity_guard.is_some()
            && request.virtual_source.is_none()
            && request.batch_rename_plan.is_none()
            && request.move_identity_guard.is_none()
            && request.rename_reparse_guard.is_none()
            && request.move_destination_identity_guard.is_none()
            && request.source_cleanup_identity_guard.is_none()
            && request.copy_destination_identity_guard.is_none()
            && request.trash_source_guards.is_none()
            && request.zip_create_guard.is_none()
            && request.seven_zip_create_guard.is_none()
            && request.seven_zip_extract_guard.is_none();
        let symbolic_link_parent_shape_is_valid = request.kind
            == super::model::OperationKind::CreateSymbolicLink
            && !request.sources.is_empty()
            && request.destination.is_some()
            && request.create_parent_directory_identity_guard.is_some()
            && request.virtual_source.is_none()
            && request.batch_rename_plan.is_none()
            && request.move_identity_guard.is_none()
            && request.rename_reparse_guard.is_none()
            && request.move_destination_identity_guard.is_none()
            && request.source_cleanup_identity_guard.is_none()
            && request.copy_destination_identity_guard.is_none()
            && request.trash_source_guards.is_none()
            && request.zip_create_guard.is_none()
            && request.seven_zip_create_guard.is_none()
            && request.seven_zip_extract_guard.is_none();
        let zip_create_shape_is_valid = request.zip_create_shape_is_valid()
            && request.virtual_source.is_none()
            && request.batch_rename_plan.is_none()
            && request.move_identity_guard.is_none()
            && request.rename_reparse_guard.is_none()
            && request.move_destination_identity_guard.is_none()
            && request.source_cleanup_identity_guard.is_none()
            && request.copy_destination_identity_guard.is_none()
            && request.create_parent_directory_identity_guard.is_none();
        let seven_zip_create_shape_is_valid = request.seven_zip_create_shape_is_valid()
            && request.batch_rename_plan.is_none()
            && request.move_identity_guard.is_none()
            && request.rename_reparse_guard.is_none()
            && request.move_destination_identity_guard.is_none()
            && request.source_cleanup_identity_guard.is_none()
            && request.copy_destination_identity_guard.is_none()
            && request.create_parent_directory_identity_guard.is_none();
        let seven_zip_extract_shape_is_valid = request.seven_zip_extract_shape_is_valid()
            && request.batch_rename_plan.is_none()
            && request.move_identity_guard.is_none()
            && request.rename_reparse_guard.is_none()
            && request.move_destination_identity_guard.is_none()
            && request.source_cleanup_identity_guard.is_none()
            && request.copy_destination_identity_guard.is_none()
            && request.create_parent_directory_identity_guard.is_none();
        let recovery_copy_guard_is_valid = request.kind == super::model::OperationKind::Copy
            && request.virtual_source.is_none()
            && !request.sources.is_empty()
            && request.sources.len() <= 1024
            && request.destination.is_some()
            && request.copy_destination_identity_guard.is_some()
            && request.move_identity_guard.as_ref().is_some_and(|guard| {
                guard.source_identities.len() == request.sources.len()
                    && guard.source_identities.iter().all(|identity| identity.windows_parts().is_some())
                    && request.copy_destination_identity_guard.as_ref() == Some(&guard.destination_identity)
            });
        let recovery_copy_policy_is_valid = request.initial_conflict_policy
            == Some(super::model::ConflictDecision::KeepBoth)
            && recovery_copy_guard_is_valid;
        if request.initial_conflict_policy.is_some() && !recovery_copy_policy_is_valid {
            return Err(SubmitError::InvalidRequest);
        }
        if request.move_identity_guard.is_some()
            && request.kind != super::model::OperationKind::Move
            && request.kind != super::model::OperationKind::Rename
            && !recovery_copy_policy_is_valid
        {
            return Err(SubmitError::InvalidRequest);
        }
        let zip_extract_shape_is_valid =
            !request.sources.is_empty()
                && request.sources.len() <= 1024
                && request.sources.iter().all(|source| {
                    source
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
                        && source.parent() == request.sources[0].parent()
                })
                && request.destination.as_ref().is_some_and(|destination| {
                    destination.parent() == request.sources[0].parent()
                        && destination
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| !name.is_empty())
                        && destination.file_name() == request.sources[0].file_stem()
                })
                && request
                    .zip_create_guard
                    .as_ref()
                    .is_some_and(|guard| match guard {
                        super::model::ZipOperationGuard::Extract(guard) => {
                            guard.source_kind == crate::directory::EntryKind::File
                                && guard.source_identity.windows_parts().is_some()
                                && guard.destination_parent_identity.windows_parts().is_some()
                                && request.sources[0]
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    == Some(guard.entry_name.as_str())
                        }
                        super::model::ZipOperationGuard::ExtractMany(guard) => {
                            guard.destination_parent_identity.windows_parts().is_some()
                                && guard.sources.len() == request.sources.len()
                                && guard.sources.iter().zip(&request.sources).all(
                                    |(guard, source)| {
                                        guard.source_kind == crate::directory::EntryKind::File
                                            && guard.source_identity.windows_parts().is_some()
                                            && guard.entry_name
                                                == source
                                                    .file_name()
                                                    .and_then(|name| name.to_str())
                                                    .unwrap_or("")
                                            && source.parent() == request.sources[0].parent()
                                            && source
                                                .extension()
                                                .and_then(|ext| ext.to_str())
                                                .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
                                    },
                                )
                        }
                        super::model::ZipOperationGuard::Create(_) => false,
                    })
                && request.virtual_source.is_none()
                && request.batch_rename_plan.is_none()
                && request.move_identity_guard.is_none()
                && request.rename_reparse_guard.is_none()
                && request.move_destination_identity_guard.is_none()
                && request.source_cleanup_identity_guard.is_none()
                && request.copy_destination_identity_guard.is_none()
                && request.create_parent_directory_identity_guard.is_none();
        if (matches!(
            request.kind,
            super::model::OperationKind::CreateFolder | super::model::OperationKind::CreateFile
        ) && !create_parent_shape_is_valid)
            || (!matches!(
                request.kind,
                super::model::OperationKind::CreateFolder | super::model::OperationKind::CreateFile
            ) && request.kind != super::model::OperationKind::CreateSymbolicLink
                && request.create_parent_directory_identity_guard.is_some())
            || (request.kind == super::model::OperationKind::CreateSymbolicLink
                && request.create_parent_directory_identity_guard.is_some()
                && !symbolic_link_parent_shape_is_valid)
            || (request.kind == super::model::OperationKind::CreateZip
                && !zip_create_shape_is_valid)
            || (request.kind == super::model::OperationKind::Create7z
                && !seven_zip_create_shape_is_valid)
            || (request.kind == super::model::OperationKind::ExtractZip
                && !zip_extract_shape_is_valid)
            || (request.kind == super::model::OperationKind::Extract7z
                && !seven_zip_extract_shape_is_valid)
            || (!matches!(
                request.kind,
                super::model::OperationKind::CreateZip | super::model::OperationKind::ExtractZip
            ) && request.zip_create_guard.is_some())
            || (request.kind != super::model::OperationKind::Create7z
                && request.seven_zip_create_guard.is_some())
            || (request.kind != super::model::OperationKind::Extract7z
                && request.seven_zip_extract_guard.is_some())
        {
            return Err(SubmitError::InvalidRequest);
        }
        let result = self.service.submit(request);
        self.seven_zip_extract_batches
            .retain(|token, _| self.service.job(token.job_id()).is_some());
        result
    }

    /// Admits a fresh retry attempt for a job through the Runtime owner.
    pub fn retry(&mut self, expected: AttemptToken) -> Result<AttemptToken, RetryError> {
        let job_id = expected.job_id();
        if self.run_id.is_none() {
            return Err(RetryError::NotRetryable);
        }
        if self
            .service
            .job(job_id)
            .is_some_and(|job| job.request.virtual_source.is_some())
        {
            return Err(RetryError::NotRetryable);
        }
        let is_batch_undo = self
            .service
            .job(job_id)
            .is_some_and(|job| job.request.kind == super::model::OperationKind::BatchRenameUndo);
        let undo_association = if is_batch_undo {
            let association = self
                .batch_undo_receipts
                .get(&job_id)
                .ok_or(RetryError::NotRetryable)?;
            let is_current = self.undo_ledger.last().is_some_and(|entry| {
                matches!(
                    entry,
                    UndoEntry::BatchRename { receipt_id, targets }
                        if *receipt_id == association.receipt_id
                            && *targets == association.targets
                )
            });
            if !is_current {
                return Err(RetryError::NotRetryable);
            }
            Some(association.clone())
        } else {
            None
        };
        let token = self.service.retry(expected)?;
        if let Some(association) = undo_association {
            self.pending_batch_undo.insert(
                token,
                PendingBatchUndo {
                    association,
                    must_still_be_current: true,
                },
            );
        }
        Ok(token)
    }

    pub fn acknowledge_batch_rename_item(
        &mut self,
        token: AttemptToken,
        item: super::model::ItemId,
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> bool {
        if self.running != Some(token)
            || self.pending_batch_rename_items.get(&token)
                != Some(&(item, source.to_path_buf(), destination.to_path_buf()))
        {
            return false;
        }
        let Some(control) = self.controls.get(&token) else {
            return false;
        };
        if control
            .send(WorkerControl::BatchRenameItemJournaledExact {
                token,
                item,
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
            })
            .is_err()
        {
            return false;
        }
        self.pending_batch_rename_items.remove(&token);
        true
    }

    pub fn submit_batch_rename(
        &mut self,
        plan: Vec<super::batch_rename::RenameTarget>,
        origin: Option<super::model::OperationOrigin>,
    ) -> Result<AttemptToken, SubmitError> {
        if plan.is_empty()
            || plan.iter().any(|item| {
                item.source.parent() != item.destination.parent()
                    || item.destination.file_name().is_none()
            })
        {
            return Err(SubmitError::InvalidRequest);
        }
        let sources = plan.iter().map(|item| item.source.clone()).collect();
        self.submit(OperationRequest {
            kind: super::model::OperationKind::BatchRename,
            virtual_source: None,
            sources,
            destination: None,
            batch_rename_plan: Some(plan),
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
            origin,
        })
    }

    pub fn submit_batch_rename_undo(
        &mut self,
        receipt_id: BatchRenameUndoId,
        original: Vec<super::batch_rename::RenameTarget>,
        origin: Option<super::model::OperationOrigin>,
    ) -> Result<AttemptToken, SubmitError> {
        let reverse_plan: Vec<_> = original
            .iter()
            .rev()
            .map(|item| super::batch_rename::RenameTarget {
                source: item.destination.clone(),
                destination: item.source.clone(),
                source_identity: item.source_identity.clone(),
                parent_identity: item.parent_identity.clone(),
                kind: item.kind,
                reparse_guard: item.reparse_guard.clone(),
            })
            .collect();
        if reverse_plan.is_empty() {
            return Err(SubmitError::InvalidRequest);
        }
        let sources = reverse_plan
            .iter()
            .map(|item| item.source.clone())
            .collect();
        let token = self.submit(OperationRequest {
            kind: super::model::OperationKind::BatchRenameUndo,
            virtual_source: None,
            sources,
            destination: None,
            batch_rename_plan: Some(reverse_plan),
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
            origin,
        })?;
        self.pending_batch_undo.insert(
            token,
            PendingBatchUndo {
                association: BatchUndoAssociation {
                    receipt_id,
                    targets: original.clone(),
                },
                must_still_be_current: false,
            },
        );
        self.batch_undo_receipts.insert(
            token.job_id(),
            BatchUndoAssociation {
                receipt_id,
                targets: original,
            },
        );
        Ok(token)
    }

    /// Admits an attempt against normal capacity but withholds execution until released.
    pub fn submit_held(&mut self, request: OperationRequest) -> Result<AttemptToken, SubmitError> {
        if self.run_id.is_none() {
            return Err(SubmitError::IdentityUnavailable);
        }
        self.service.submit_held(request)
    }

    /// Makes this exact admitted attempt runnable once; stale and duplicate tokens are ignored.
    pub fn release_held(&mut self, token: AttemptToken) -> bool {
        self.service.release_held(token)
    }

    pub fn reject_held(&mut self, token: AttemptToken) -> bool {
        self.service.reject_held(token)
    }

    pub fn resolve_move_identity_guard(
        &mut self,
        token: AttemptToken,
        guard: super::model::MoveIdentityGuard,
    ) -> bool {
        self.service.resolve_move_identity_guard(token, guard)
    }

    pub fn submit_external_move_cleanup(
        &mut self,
        request: OperationRequest,
        lease: SourceCleanupLease,
    ) -> Result<AttemptToken, SourceCleanupSubmitError> {
        if self.run_id.is_none() {
            return Err(SourceCleanupSubmitError::Rejected(
                SubmitError::IdentityUnavailable,
            ));
        }
        let valid = request.kind == super::model::OperationKind::ExternalMoveCleanup
            && request.destination.is_none()
            && request.move_identity_guard.is_none()
            && !request.sources.is_empty()
            && request
                .source_cleanup_identity_guard
                .as_ref()
                .is_some_and(|guard| {
                    guard.source_identities.len() == request.sources.len()
                        && valid_cleanup_parent_routes(&request.sources, &guard.parent_routes)
                        && guard.parent_routes == lease.parent_routes
                        && guard.leaf_signatures.len() == request.sources.len()
                        && guard.leaf_signatures == lease.leaf_signatures
                        && guard
                            .leaf_signatures
                            .iter()
                            .all(|signature| signature.number_of_links > 0)
                })
            && request.sources == lease.paths
            && request.sources.len() == lease.handles.len()
            && request.sources.len() == lease.parent_routes.len()
            && request.sources.len() == lease.leaf_signatures.len();
        if !valid {
            drop(lease);
            return Err(SourceCleanupSubmitError::InvalidLease);
        }
        let token = self
            .service
            .submit(request)
            .map_err(SourceCleanupSubmitError::Rejected)?;
        self.source_cleanup_leases.insert(token, lease);
        Ok(token)
    }

    /// One receipt is consumed per Ctrl+Z.  It is never reconstructed from a
    /// display path: the adapter's opaque recycle identity remains intact.
    pub fn reserve_undo(&mut self) -> Option<(usize, UndoEntry)> {
        if self.running.is_some() || self.undo_reservation.is_some() {
            return None;
        }
        let index = self.undo_ledger.len().checked_sub(1)?;
        let receipt = self.undo_ledger.get(index)?.clone();
        self.undo_reservation = Some(index);
        Some((index, receipt))
    }

    pub(crate) fn undo_top_is_directory_copy(&self) -> bool {
        matches!(self.undo_ledger.last(), Some(UndoEntry::DirectoryCopy(_) | UndoEntry::DirectoryCopyGroup(_)))
            || matches!(self.undo_ledger.last(), Some(UndoEntry::MixedCopyGroup(items))
                if items.iter().any(|item| matches!(item, super::model::CopyUndoItem::Directory(_))))
    }

    pub(crate) fn undo_entry_matches_directory_copy(
        &self,
        index: usize,
        receipt: &super::model::DirectoryCopyUndoReceipt,
    ) -> bool {
        self.undo_reservation == Some(index)
            && index + 1 == self.undo_ledger.len()
            && matches!(self.undo_ledger.get(index), Some(UndoEntry::DirectoryCopy(actual)) if actual == receipt)
    }

    pub(crate) fn undo_entry_matches_directory_copy_group(
        &self,
        index: usize,
        receipts: &[super::model::DirectoryCopyUndoReceipt],
    ) -> bool {
        self.undo_reservation == Some(index)
            && index + 1 == self.undo_ledger.len()
            && matches!(self.undo_ledger.get(index), Some(UndoEntry::DirectoryCopyGroup(actual)) if actual == receipts)
    }

    pub(crate) fn finish_directory_copy_group_undo(&mut self, index: usize, consumed_reverse_suffix: usize) -> bool {
        if self.undo_reservation != Some(index) || index + 1 != self.undo_ledger.len() { return false; }
        let Some(UndoEntry::DirectoryCopyGroup(receipts)) = self.undo_ledger.get_mut(index) else { return false; };
        if consumed_reverse_suffix > receipts.len() { return false; }
        self.undo_reservation = None;
        receipts.truncate(receipts.len() - consumed_reverse_suffix);
        if receipts.is_empty() { self.undo_ledger.pop(); }
        true
    }

    pub(crate) fn undo_entry_matches_trash_group(&self, index: usize, original_paths: &[std::path::PathBuf]) -> bool {
        self.undo_reservation == Some(index)
            && index + 1 == self.undo_ledger.len()
            && matches!(self.undo_ledger.get(index), Some(UndoEntry::TrashGroup(receipts))
                if !receipts.is_empty() && receipts.iter().map(|receipt| receipt.original_path()).collect::<Vec<_>>() == original_paths)
    }

    pub(crate) fn finish_trash_group_undo(&mut self, index: usize, original_paths: &[std::path::PathBuf], consumed_reverse_suffix: usize) -> bool {
        if !self.undo_entry_matches_trash_group(index, original_paths) || consumed_reverse_suffix > original_paths.len() {
            return false;
        }
        let Some(UndoEntry::TrashGroup(receipts)) = self.undo_ledger.get_mut(index) else { return false; };
        self.undo_reservation = None;
        receipts.truncate(receipts.len() - consumed_reverse_suffix);
        if receipts.is_empty() { self.undo_ledger.pop(); }
        true
    }

    pub(crate) fn undo_entry_matches_mixed_copy_group(&self, index: usize, items: &[super::model::CopyUndoItem]) -> bool {
        self.undo_reservation == Some(index)
            && index + 1 == self.undo_ledger.len()
            && matches!(self.undo_ledger.get(index), Some(UndoEntry::MixedCopyGroup(actual)) if actual == items)
    }

    pub(crate) fn finish_mixed_copy_group_undo(&mut self, index: usize, consumed_prefix: usize) -> bool {
        if self.undo_reservation != Some(index) || index + 1 != self.undo_ledger.len() { return false; }
        let Some(UndoEntry::MixedCopyGroup(items)) = self.undo_ledger.get_mut(index) else { return false; };
        if consumed_prefix > items.len() { return false; }
        self.undo_reservation = None;
        drop(items.drain(..consumed_prefix));
        if items.is_empty() { self.undo_ledger.pop(); }
        true
    }

    #[cfg(test)]
    pub fn undo_snapshot_for_test(&self) -> (bool, Vec<UndoEntry>) {
        (self.undo_reservation.is_some(), self.undo_ledger.clone())
    }

    #[cfg(test)]
    pub fn move_undo_candidate_for_test(
        &self,
        token: AttemptToken,
        item: super::model::ItemId,
    ) -> Option<super::model::MoveUndoReceipt> {
        self.move_undo_candidates.get(&(token, item)).cloned()
    }

    #[cfg(test)]
    pub(crate) fn push_undo_for_test(&mut self, entry: UndoEntry) {
        self.undo_ledger.push(entry);
    }

    /// A trash restore holds the serial lane until its terminal result. Failure
    /// retains the receipt at its original LIFO position for an explicit retry.
    pub fn finish_trash_undo(&mut self, index: usize, restored: bool) -> bool {
        if self.undo_reservation != Some(index) || index + 1 != self.undo_ledger.len() {
            return false;
        }
        self.undo_reservation = None;
        if restored {
            self.undo_ledger.pop();
        }
        true
    }

    pub fn finish_move_undo(&mut self, index: usize, restored_suffix: usize) -> bool {
        if self.undo_reservation != Some(index) || index + 1 != self.undo_ledger.len() {
            return false;
        }
        let Some(UndoEntry::Move(receipts)) = self.undo_ledger.get_mut(index) else {
            return false;
        };
        if restored_suffix > receipts.len() {
            return false;
        }
        self.undo_reservation = None;
        receipts.truncate(receipts.len() - restored_suffix);
        if receipts.is_empty() {
            self.undo_ledger.pop();
        }
        true
    }

    pub fn return_undo(&mut self, receipt: UndoEntry) {
        if self.undo_reservation.is_none() {
            self.undo_ledger.push(receipt);
            self.trim_undo_ledger();
        }
    }

    pub fn acknowledge_batch_rename_plan(
        &mut self,
        token: AttemptToken,
        plan: &[super::batch_rename::RenameTarget],
    ) -> bool {
        let Some(expected) = self.pending_batch_plan_intents.remove(&token) else {
            return false;
        };
        let request_matches = self.service.job(token.job_id()).is_some_and(|job| {
            matches!(
                job.request.kind,
                super::model::OperationKind::BatchRename
                    | super::model::OperationKind::BatchRenameUndo
            ) && job.request.batch_rename_plan.as_deref() == Some(plan)
        });
        if self.running != Some(token) || expected != plan || !request_matches {
            return false;
        }
        self.controls.get(&token).is_some_and(|control| {
            control
                .send(WorkerControl::BatchRenamePlanJournaledExact {
                    token,
                    plan: plan.to_vec(),
                })
                .is_ok()
        })
    }

    /// Claims one queued attempt only when the previous worker has committed a
    /// terminal result.  The returned flag is allocated before the worker can
    /// start, so an immediate cancel cannot be lost.
    pub fn start_next(&mut self) -> Option<WorkerStart> {
        if self.running.is_some() || self.undo_reservation.is_some() {
            return None;
        }
        let command = self.service.start_next()?;
        let ExecutorCommand::Prepare { token, .. } = &command else {
            return None;
        };
        let token = *token;
        let seven_zip_extract_attempt = self
            .job(token)
            .filter(|job| job.request.kind == super::model::OperationKind::Extract7z)
            .and_then(|_| self.run_id)
            .map(|run_id| super::journal::JournalSevenZipExtractAttemptId {
                run_id,
                job: token.job_id().value(),
                attempt: token.attempt_id().value(),
            });
        // E2 has no asynchronous preflight boundary yet.  Marking it running
        // immediately before handing it to the background worker keeps cancel
        // and terminal facts valid without inventing a second owner.
        assert!(self.service.prepared(token));
        let cancellation = Arc::new(AtomicBool::new(false));
        let (control_sender, controls) = std::sync::mpsc::channel();
        self.cancellations.insert(token, cancellation.clone());
        self.controls.insert(token, control_sender);
        self.running = Some(token);
        let copy_stage_cleanup_lease = self.copy_stage_cleanup_leases.remove(&token);
        let copy_stage_cleanup_running_ack = if copy_stage_cleanup_lease.is_some() {
            let frozen_running = self.journal_attempt(token).filter(|record| {
                record.state == super::journal::JournalState::Running
                    && record.kind == super::journal::JournalKind::Copy
            });
            let (sender, receiver) = async_channel::bounded(1);
            if let Some(record) = frozen_running {
                self.copy_stage_cleanup_running_acks
                    .insert(token, (record, sender));
            }
            Some(receiver)
        } else {
            None
        };
        Some(WorkerStart {
            command,
            cancellation,
            controls,
            source_cleanup_lease: self.source_cleanup_leases.remove(&token),
            copy_stage_cleanup_lease,
            copy_stage_cleanup_running_ack,
            seven_zip_extract_attempt,
        })
    }

    /// Accept only one successful ACK for the exact frozen Running snapshot.
    /// Dropping the pending sender wakes the worker with no deletion authority.
    pub fn resolve_copy_stage_cleanup_running_ack(
        &mut self,
        token: AttemptToken,
        acknowledged_record: &super::journal::JournalAttempt,
        acknowledged: bool,
    ) -> bool {
        let Some((frozen, sender)) = self.copy_stage_cleanup_running_acks.remove(&token) else {
            return false;
        };
        let current = self.journal_attempt(token);
        let current_running = self.job(token).is_some_and(|job| {
            job.request.kind == super::model::OperationKind::CopyStageCleanup
                && job.attempts.last().is_some_and(|attempt| {
                    attempt.token == token
                        && attempt.state == super::model::AttemptState::Running
                        && attempt.terminal.is_none()
                })
        });
        let not_cancelled = self
            .cancellations
            .get(&token)
            .is_some_and(|cancelled| !cancelled.load(Ordering::Acquire));
        if acknowledged
            && acknowledged_record == &frozen
            && current.as_ref() == Some(&frozen)
            && self.running == Some(token)
            && current_running
            && not_cancelled
        {
            sender.try_send(()).is_ok()
        } else {
            false
        }
    }

    pub fn reject_copy_stage_cleanup_running_ack(&mut self, token: AttemptToken) {
        self.copy_stage_cleanup_running_acks.remove(&token);
    }

    /// Requests cancellation by exact attempt identity.  The worker observes
    /// the flag, but the terminal report remains the acknowledgement.
    pub fn cancel(&mut self, token: AttemptToken) -> Option<TerminalCommit> {
        if self.directory_moves.iter().any(|((attempt, _), state)| {
            *attempt == token && matches!(state.barrier, DirectoryMoveBarrier::TerminalPending)
        }) {
            // Source cleanup is already durably complete. Once the terminal
            // write is in flight, cancellation can no longer undo the move.
            return None;
        }
        match self.service.cancel(token)? {
            ServiceEventOrCommand::Command(ExecutorCommand::Cancel { token }) => {
                if let Some(flag) = self.cancellations.get(&token) {
                    flag.store(true, Ordering::Release);
                }
                self.reject_copy_stage_cleanup_running_ack(token);
                if let Some(state) = self.seven_zip_extract_batches.get_mut(&token) {
                    state.cancel_requested = true;
                }
                if let Some(control) = self.controls.get(&token) {
                    let _ = control.send(WorkerControl::Cancel);
                }
                None
            }
            ServiceEventOrCommand::Event(event) => Some(self.accept_terminal(event)),
            ServiceEventOrCommand::Command(_) => None,
        }
    }

    /// A conflict answer is accepted only by the model's complete opaque token.
    /// The current E2 executor has no conflicts yet, so resolution is retained
    /// as an explicit command boundary for the later adapter.
    pub fn resolve_conflict(
        &mut self,
        token: ConflictToken,
        decision: ConflictDecision,
        apply_to_remaining: bool,
    ) -> bool {
        let attempt = token.attempt_token();
        let item = token.item_id();
        let move_item_count = self.job(attempt).and_then(|job| {
            (job.request.kind == super::model::OperationKind::Move).then_some((
                job.request.sources.len(),
                job.request.move_identity_guard.clone(),
            ))
        });
        let keep_both_item_count = self.job(attempt).and_then(|job| {
            matches!(job.request.kind, super::model::OperationKind::Move | super::model::OperationKind::Copy).then_some((
                job.request.sources.len(),
                job.request.move_identity_guard.clone(),
            ))
        });
        if decision == ConflictDecision::Replace
            && move_item_count.as_ref().is_some_and(|(count, guard)| {
                *count > 1
                    && !guard.as_ref().is_some_and(|guard| {
                        guard
                            .source_identities
                            .first()
                            .and_then(|id| id.windows_parts())
                            .zip(guard.destination_identity.windows_parts())
                            .is_some_and(|((source_volume, _), (destination_volume, _))| {
                                source_volume == destination_volume
                            })
                    })
            })
        {
            return false;
        }
        if self.running != Some(attempt) {
            return false;
        }
        if !self.service.can_resolve_conflict(token, decision) {
            return false;
        }
        let active = attempt;
        if !self.controls.contains_key(&active) {
            return false;
        }
        let committed = matches!(
            self.service.resolve_conflict(token, decision),
            Some(ExecutorCommand::ResolveConflict { .. })
        );
        debug_assert!(committed, "validated conflict changed before commit");
        if !committed {
            return false;
        }
        let approved_directory_candidate = (decision == ConflictDecision::Replace
            && !apply_to_remaining)
            .then(|| self.service.accepted_directory_move_replace_rename(token))
            .flatten();
        let Some(control) = self.controls.get(&active) else {
            return false;
        };
        let send_result = if let Some(candidate) = approved_directory_candidate {
            control.send(WorkerControl::DirectoryMoveReplaceRenameApproved {
                token,
                candidate,
            })
        } else {
            control.send(WorkerControl::Resolve {
                decision,
                apply_to_remaining,
            })
        };
        if send_result.is_err() {
            return false;
        }
        if let Some((item_count, _)) = keep_both_item_count {
            if decision == ConflictDecision::KeepBoth {
                let end = if apply_to_remaining {
                    item_count
                } else {
                    (item.0 as usize).saturating_add(1).min(item_count)
                };
                for index in item.0 as usize..end {
                    self.keep_both_directory_items
                        .insert((active, super::model::ItemId(index as u32)));
                }
            } else {
                self.keep_both_directory_items
                    .retain(|(authorized_attempt, authorized_item)| {
                        *authorized_attempt != active || authorized_item.0 < item.0
                    });
            }
        }
        true
    }

    /// Returns only the candidate approved by this exact Replace conflict token.
    /// A journal payload cannot create or widen this approval.
    pub(crate) fn directory_move_replace_rename_approval(
        &self,
        token: ConflictToken,
    ) -> Option<super::model::DirectoryMoveReplaceRenameCandidate> {
        self.service.accepted_directory_move_replace_rename(token)
    }

    /// Escape/close for the dialog.  A stale dialog cannot set a newer
    /// attempt's flag because the model validates the full conflict token.
    pub fn cancel_conflict(&mut self, token: ConflictToken) -> bool {
        let Some(ExecutorCommand::Cancel { token }) = self.service.cancel_conflict(token) else {
            return false;
        };
        if let Some(flag) = self.cancellations.get(&token) {
            flag.store(true, Ordering::Release);
        }
        if let Some(control) = self.controls.get(&token) {
            let _ = control.send(WorkerControl::Cancel);
        }
        true
    }

    /// Commits detached worker facts.  Progress is lossy/latest-wins in the
    /// model; terminal is reliable and removes the cancellation flag only when
    /// the service accepts that exact current attempt.
    pub fn commit_fact(&mut self, fact: ExecutorFact) -> Option<TerminalCommit> {
        match fact {
            ExecutorFact::DirectoryMoveManifestIntent { token, ledger } => {
                let current = self.running == Some(token)
                    && self
                        .cancellations
                        .get(&token)
                        .is_some_and(|flag| !flag.load(Ordering::Acquire))
                    && self.job(token).is_some_and(|job| {
                        job.attempts.iter().any(|attempt| {
                            attempt.token == token
                                && attempt.state == super::model::AttemptState::Running
                        }) && job.request.kind == super::model::OperationKind::Move
                            && job
                                .request
                                .sources
                                .get(ledger.item as usize)
                                .is_some_and(|source| {
                                    super::journal::JournalPath::from_path(source) == ledger.source
                                })
                            && job.request.destination.as_ref().is_some_and(|parent| {
                                let source = &job.request.sources[ledger.item as usize];
                                let base = parent.join(source.file_name().unwrap_or_default());
                                super::journal::JournalPath::from_path(&base) == ledger.destination
                                    || (self
                                        .keep_both_directory_items
                                        .contains(&(token, super::model::ItemId(ledger.item)))
                                        && (2..10_000).any(|suffix| {
                                            super::model::keep_both_candidate_path(&base, suffix)
                                                .is_some_and(|candidate| {
                                                    super::journal::JournalPath::from_path(
                                                        &candidate,
                                                    ) == ledger.destination
                                                })
                                        }))
                            })
                    });
                if current {
                    if let Some(job) = self.job(token) {
                        let key = (token, super::model::ItemId(ledger.item));
                        let source = job.request.sources[ledger.item as usize].clone();
                        let base_destination = job
                            .request
                            .destination
                            .as_ref()
                            .unwrap()
                            .join(source.file_name().unwrap_or_default());
                        let authorized_keep_both = self.keep_both_directory_items.contains(&key);
                        let destination =
                            if super::journal::JournalPath::from_path(&base_destination)
                                == ledger.destination
                            {
                                Some(base_destination)
                            } else if authorized_keep_both {
                                (2..10_000).find_map(|suffix| {
                                    let candidate = super::model::keep_both_candidate_path(
                                        &base_destination,
                                        suffix,
                                    )?;
                                    (super::journal::JournalPath::from_path(&candidate)
                                        == ledger.destination)
                                        .then_some(candidate)
                                })
                            } else {
                                None
                            };
                        if let Some(destination) = destination
                            && !self.directory_moves.contains_key(&key)
                        {
                            self.directory_moves.insert(
                                key,
                                DirectoryMoveRuntimeState {
                                    item: super::model::ItemId(ledger.item),
                                    source,
                                    destination,
                                    barrier: DirectoryMoveBarrier::ManifestPending(ledger),
                                },
                            );
                        }
                    }
                }
                None
            }
            ExecutorFact::DirectoryMoveChildIntent {
                token,
                item,
                intent,
            } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_moves
                        .get(&(token, item))
                        .is_some_and(|state| matches!(state.barrier, DirectoryMoveBarrier::Ready))
                {
                    self.directory_moves
                        .get_mut(&(token, item))
                        .unwrap()
                        .barrier = DirectoryMoveBarrier::ChildIntentPending(intent);
                }
                None
            }
            ExecutorFact::DirectoryMoveChildDisposition {
                token,
                item,
                intent,
            } => {
                if self.directory_move_is_current(token, item)
                    && self.directory_moves.get(&(token, item)).is_some_and(|state|
                        matches!(&state.barrier, DirectoryMoveBarrier::ChildIntentDurable(pending) if pending == &intent))
                {
                    self.directory_moves.get_mut(&(token, item)).unwrap().barrier =
                        DirectoryMoveBarrier::ChildReceiptPending(intent);
                }
                None
            }
            ExecutorFact::DirectoryMoveRootIntent {
                token,
                item,
                intent,
            } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_moves
                        .get(&(token, item))
                        .is_some_and(|state| matches!(state.barrier, DirectoryMoveBarrier::Ready))
                {
                    self.directory_moves
                        .get_mut(&(token, item))
                        .unwrap()
                        .barrier = DirectoryMoveBarrier::RootIntentPending(intent);
                }
                None
            }
            ExecutorFact::DirectoryMoveRootDisposition {
                token,
                item,
                intent,
            } => {
                if self.directory_move_is_current(token, item)
                    && self.directory_moves.get(&(token, item)).is_some_and(|state|
                        matches!(&state.barrier, DirectoryMoveBarrier::RootIntentDurable(pending) if pending == &intent))
                {
                    self.directory_moves.get_mut(&(token, item)).unwrap().barrier =
                        DirectoryMoveBarrier::RootReceiptPending(intent);
                }
                None
            }
            ExecutorFact::DirectoryMoveComplete { token, item } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_moves
                        .get(&(token, item))
                        .is_some_and(|state| matches!(state.barrier, DirectoryMoveBarrier::Ready))
                {
                    self.directory_moves
                        .get_mut(&(token, item))
                        .unwrap()
                        .barrier = DirectoryMoveBarrier::CompletePending;
                }
                None
            }
            ExecutorFact::DirectoryMoveReplacePhaseIntent { token, item, phase } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_move_replaces
                        .get(&(token, item))
                        .is_some_and(|state| {
                            (state.barrier == DirectoryMoveReplaceBarrier::Ready
                                || (phase == super::journal::DirectoryMoveReplacePhase::Complete
                                    && state.barrier
                                        == DirectoryMoveReplaceBarrier::BackupComplete))
                                && super::journal::directory_move_replace_phase_next(
                                    state.phase,
                                    phase,
                                )
                        })
                {
                    self.directory_move_replaces
                        .get_mut(&(token, item))
                        .unwrap()
                        .barrier = DirectoryMoveReplaceBarrier::PhasePending(phase);
                }
                None
            }
            ExecutorFact::DirectoryMoveReplaceBackupIntent {
                token,
                item,
                intent,
            } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_move_replaces
                        .get(&(token, item))
                        .is_some_and(|state| {
                            state.phase == super::journal::DirectoryMoveReplacePhase::BackupCleanup
                                && state.barrier == DirectoryMoveReplaceBarrier::Ready
                                && directory_move_replace_backup_next_intent(&state.ledger, &intent)
                        })
                {
                    let state = self
                        .directory_move_replaces
                        .get_mut(&(token, item))
                        .unwrap();
                    state.ledger.backup_pending = Some(intent.clone());
                    state.barrier = DirectoryMoveReplaceBarrier::BackupIntentPending(intent);
                }
                None
            }
            ExecutorFact::DirectoryMoveReplaceBackupDisposition {
                token,
                item,
                intent,
            } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_move_replaces
                        .get(&(token, item))
                        .is_some_and(|state| {
                            state.ledger.backup_pending.as_ref() == Some(&intent)
                                && state.barrier
                                    == DirectoryMoveReplaceBarrier::BackupIntentDurable(
                                        intent.clone(),
                                    )
                        })
                {
                    self.directory_move_replaces
                        .get_mut(&(token, item))
                        .unwrap()
                        .barrier = DirectoryMoveReplaceBarrier::BackupReceiptPending(intent);
                }
                None
            }
            ExecutorFact::DirectoryMoveReplaceBackupComplete { token, item } => {
                if self.directory_move_is_current(token, item)
                    && self
                        .directory_move_replaces
                        .get(&(token, item))
                        .is_some_and(|state| {
                            state.phase == super::journal::DirectoryMoveReplacePhase::BackupCleanup
                                && state.barrier == DirectoryMoveReplaceBarrier::Ready
                                && state.ledger.backup_pending.is_none()
                                && state.ledger.backup_root_disposition_recorded
                                && state
                                    .ledger
                                    .backup_children
                                    .iter()
                                    .all(|child| child.disposition_recorded)
                        })
                {
                    self.directory_move_replaces
                        .get_mut(&(token, item))
                        .unwrap()
                        .barrier = DirectoryMoveReplaceBarrier::BackupCompletePending;
                }
                None
            }
            ExecutorFact::CopyReplaceCleanupManifestIntent { token, ledger } => {
                let item = super::model::ItemId(ledger.item);
                let valid = self.running == Some(token)
                    && self
                        .cancellations
                        .get(&token)
                        .is_some_and(|flag| !flag.load(Ordering::Acquire))
                    && item == super::model::ItemId(0)
                    && self.pending_replace_intents.get(&token).is_some_and(
                        |(pending_item, source, destination, backup)| {
                            *pending_item == item
                                && self.service.job(token.job_id()).is_some_and(|job| {
                                    job.request.kind == super::model::OperationKind::Copy
                                        && job.request.sources.len() == 1
                                        && job.request.sources.first() == Some(source)
                                        && job.request.destination.as_ref().is_some_and(|parent| {
                                            parent.join(source.file_name().unwrap_or_default())
                                                == *destination
                                        })
                                })
                                && super::journal::JournalPath::from_path(destination)
                                    == ledger.destination
                                && super::journal::JournalPath::from_path(backup) == ledger.backup
                        },
                    );
                if valid && !self.copy_replace_cleanups.contains_key(&(token, item)) {
                    self.copy_replace_cleanups.insert(
                        (token, item),
                        CopyReplaceCleanupRuntimeState {
                            ledger,
                            barrier: CopyReplaceCleanupBarrier::ManifestPending,
                        },
                    );
                }
                None
            }
            ExecutorFact::CopyReplaceCleanupChildIntent {
                token,
                item,
                intent,
            } => {
                if self.copy_replace_cleanup_current(token, item) {
                    if let Some(state) = self.copy_replace_cleanups.get_mut(&(token, item)) {
                        let root = intent.ordinal as usize == state.ledger.children.len()
                            && intent.relative_components.is_empty()
                            && intent.identity == state.ledger.root_identity
                            && intent.content_fact == "root";
                        let expected = if root {
                            state
                                .ledger
                                .children
                                .iter()
                                .all(|child| child.disposition_recorded)
                        } else {
                            state
                                .ledger
                                .children
                                .get(intent.ordinal as usize)
                                .is_some_and(|child| {
                                    !child.disposition_recorded
                                        && child.ordinal == intent.ordinal
                                        && child.node_kind == intent.node_kind
                                        && child.relative_components == intent.relative_components
                                        && child.identity == intent.identity
                                        && child.content_fact == intent.content_fact
                                })
                        };
                        if state.barrier == CopyReplaceCleanupBarrier::Ready
                            && expected
                            && state.ledger.pending.is_none()
                        {
                            state.ledger.pending = Some(intent.clone());
                            state.barrier = CopyReplaceCleanupBarrier::IntentPending(intent);
                        }
                    }
                }
                None
            }
            ExecutorFact::CopyReplaceCleanupChildDisposition {
                token,
                item,
                intent,
            } => {
                if let Some(state) = self.copy_replace_cleanups.get_mut(&(token, item)) {
                    if state.barrier == CopyReplaceCleanupBarrier::IntentDurable(intent.clone())
                        && state.ledger.pending.as_ref() == Some(&intent)
                    {
                        state.barrier = CopyReplaceCleanupBarrier::ReceiptPending(intent);
                    }
                }
                None
            }
            ExecutorFact::CopyReplaceCleanupComplete { token, item } => {
                if let Some(state) = self.copy_replace_cleanups.get_mut(&(token, item)) {
                    if state.barrier == CopyReplaceCleanupBarrier::Ready
                        && state.ledger.pending.is_none()
                        && state.ledger.root_disposition_recorded
                        && state
                            .ledger
                            .children
                            .iter()
                            .all(|child| child.disposition_recorded)
                    {
                        state.barrier = CopyReplaceCleanupBarrier::CompletePending;
                    }
                }
                None
            }
            ExecutorFact::BatchRenamePlanIntent { token, plan } => {
                let valid = self.running == Some(token)
                    && self.service.job(token.job_id()).is_some_and(|job| {
                        matches!(
                            job.request.kind,
                            super::model::OperationKind::BatchRename
                                | super::model::OperationKind::BatchRenameUndo
                        ) && job.request.batch_rename_plan.as_deref() == Some(plan.as_slice())
                    });
                if valid {
                    self.pending_batch_plan_intents.insert(token, plan);
                }
                None
            }
            ExecutorFact::MoveGuardResolved { token, guard } => {
                if self.running == Some(token) {
                    self.resolve_move_identity_guard(token, guard);
                }
                None
            }
            ExecutorFact::GuardedMoveNoEffectProof { token, proof } => {
                if self.running == Some(token) {
                    self.service
                        .record_guarded_move_no_effect_proof(token, proof);
                }
                None
            }
            ExecutorFact::CrossVolumeMoveNoEffectProof { token, proof } => {
                if self.running == Some(token) {
                    self.service.record_cross_volume_move_no_effect_proof(token, proof);
                }
                None
            }
            ExecutorFact::PublishedMoveSourceRetained { token, proof } => {
                if self.running == Some(token) {
                    self.service.record_published_move_source_retained(token, proof);
                }
                None
            }
            ExecutorFact::PublishedMoveDisposition { token, proof } => {
                if self.running == Some(token) { self.service.record_published_move_disposition(token, proof); }
                None
            }
            ExecutorFact::CopyFailureProvenance { token, provenance } => {
                if self.running == Some(token) {
                    self.service
                        .record_copy_failure_provenance(token, provenance);
                }
                None
            }
            ExecutorFact::CopyDirectoryRetryProvenance { token, provenance } => {
                let first_attempt_kind = self.ordinary_copy_source_kinds.get(&(token, provenance.item))
                    == Some(&super::executor::OrdinaryCopySourceKind::Directory);
                let guarded_retry_kind = self.service.job(token.job_id()).is_some_and(|job| {
                    job.request.copy_destination_identity_guard.is_some()
                        && matches!(self.ordinary_copy_source_kinds_by_job.get(&token.job_id()),
                            Some(OrdinaryCopyRequestClassification::AllDirectories | OrdinaryCopyRequestClassification::Mixed))
                        && job.attempts.last().is_some_and(|attempt| {
                            attempt.token == token && attempt.retry_items.contains(&provenance.item)
                                && attempt.copy_directory_retry_guards.iter().any(|guard| {
                                    guard.item == provenance.item && guard.source == provenance.source
                                        && guard.destination == provenance.destination
                                })
                        })
                });
                if self.running == Some(token) && (first_attempt_kind || guarded_retry_kind) {
                    self.service.record_copy_directory_retry_provenance(token, provenance);
                }
                None
            }
            // This fact is intercepted by OperationApplicationOwner and is
            // promoted only after the exact operation worker has returned.
            ExecutorFact::CopyStageCustody { custody: _ } => None,
            ExecutorFact::TrashReceipt { token, item, receipt } => {
                if self.running == Some(token) {
                    self.pending_trash.entry(token).or_default().push((item, receipt));
                }
                None
            }
            ExecutorFact::ReplaceBackupIntent {
                token,
                item,
                source,
                destination,
                backup,
            } => {
                if self.running == Some(token) {
                    self.pending_replace_intents.insert(
                        token,
                        (item, source.clone(), destination.clone(), backup.clone()),
                    );
                    self.replace_backups.insert(
                        token,
                        super::journal::JournalReplaceBackup {
                            source: super::journal::JournalPath::from_path(&source),
                            destination: super::journal::JournalPath::from_path(&destination),
                            backup: super::journal::JournalPath::from_path(&backup),
                        },
                    );
                }
                None
            }
            ExecutorFact::DirectoryMoveReplaceRenamePhaseIntent {
                token,
                conflict_token,
                ledger,
            } => {
                let candidate = self.service.accepted_directory_move_replace_rename(conflict_token);
                let current = self.directory_move_replace_rename.get(&token);
                let cancelled = self.cancellations.get(&token).is_some_and(|flag| flag.load(Ordering::Acquire));
                let no_effect_after_cancel = cancelled
                    && ledger.phase == super::journal::DirectoryMoveReplaceRenamePhase::NoEffect
                    && ledger.source_location == super::journal::DirectoryMoveReplaceSourceLocation::Original
                    && ledger.target_location == super::journal::DirectoryMoveReplaceTargetLocation::Destination
                    && current.is_some_and(|previous| {
                        previous.source_location == super::journal::DirectoryMoveReplaceSourceLocation::Original
                            && match previous.phase {
                                super::journal::DirectoryMoveReplaceRenamePhase::Prepared => {
                                    previous.target_location == super::journal::DirectoryMoveReplaceTargetLocation::Destination
                                }
                                super::journal::DirectoryMoveReplaceRenamePhase::DisplaceIntent => {
                                    previous.target_location == super::journal::DirectoryMoveReplaceTargetLocation::Uncertain
                                }
                                _ => false,
                            }
                    });
                let initial_prepared_after_cancel = cancelled && current.is_none()
                    && ledger.phase == super::journal::DirectoryMoveReplaceRenamePhase::Prepared
                    && ledger.source_location == super::journal::DirectoryMoveReplaceSourceLocation::Original
                    && ledger.target_location == super::journal::DirectoryMoveReplaceTargetLocation::Destination;
                let cancelled_phase_allowed = no_effect_after_cancel || initial_prepared_after_cancel;
                let phase_valid = match current {
                    None => matches!(
                        ledger.phase,
                        super::journal::DirectoryMoveReplaceRenamePhase::Prepared
                    ),
                    Some(previous) => {
                        super::journal::directory_move_replace_rename_phase_next(previous.phase, ledger.phase)
                            && matches!(
                                ledger.phase,
                                super::journal::DirectoryMoveReplaceRenamePhase::DisplaceIntent
                                    | super::journal::DirectoryMoveReplaceRenamePhase::TargetDisplaced
                                    | super::journal::DirectoryMoveReplaceRenamePhase::PublishIntent
                                    | super::journal::DirectoryMoveReplaceRenamePhase::DestinationPublished
                                    | super::journal::DirectoryMoveReplaceRenamePhase::BackupCleanup
                                    | super::journal::DirectoryMoveReplaceRenamePhase::BackupComplete
                                    | super::journal::DirectoryMoveReplaceRenamePhase::Complete
                                    | super::journal::DirectoryMoveReplaceRenamePhase::NoEffect
                            )
                            && directory_move_replace_rename_immutable_fields_match(previous, &ledger)
                            && directory_move_replace_rename_progress_matches(previous, &ledger)
                    }
                };
                if self.running == Some(token)
                    && (!cancelled || cancelled_phase_allowed)
                    && conflict_token.attempt_token() == token
                    && conflict_token.item_id() == super::model::ItemId(ledger.item)
                    && ledger.item == 0
                    && candidate.as_ref().is_some_and(|candidate| {
                        directory_move_replace_rename_matches_candidate(candidate, &ledger)
                    })
                    && phase_valid
                    && !self.pending_directory_move_replace_rename.contains_key(&token)
                {
                    self.pending_directory_move_replace_rename
                        .insert(token, (conflict_token, ledger));
                } else if self.running == Some(token) {
                    if !cancelled { let _ = self.cancel(token); }
                }
                None
            }
            ExecutorFact::DirectoryMoveReplaceRenameCleanupIntent { token, item, intent } => {
                let valid = item == super::model::ItemId(0) && self.running == Some(token)
                    && self.cancellations.get(&token).is_some_and(|flag| !flag.load(Ordering::Acquire))
                    && self.pending_directory_move_replace_rename_cleanup.get(&token)
                        .is_none_or(|(_, barrier)| *barrier == RenameCleanupBarrier::Ready)
                    && self.directory_move_replace_rename.get(&token).is_some_and(|ledger| {
                        use super::journal::JournalDirectoryNodeKind as Kind;
                        if ledger.phase != super::journal::DirectoryMoveReplaceRenamePhase::BackupCleanup || ledger.pending_cleanup.is_some() || ledger.backup_root_disposed { return false; }
                        let count = ledger.approved_target_children.len();
                        if intent.ordinal as usize == count {
                            return intent.node_kind == Kind::Directory && intent.relative_components.is_empty()
                                && intent.identity == ledger.approved_target_identity && intent.content_fact == "root"
                                && ledger.approved_target_children.iter().all(|child| child.disposition_recorded);
                        }
                        ledger.approved_target_children.iter().find(|child| !child.disposition_recorded).is_some_and(|child| {
                            child.ordinal == intent.ordinal && child.node_kind == intent.node_kind
                                && child.relative_components == intent.relative_components && child.identity == intent.identity
                                && child.content_fact == intent.content_fact
                        })
                    });
                if valid { self.pending_directory_move_replace_rename_cleanup.insert(token, (intent, RenameCleanupBarrier::IntentPending)); }
                else if self.running == Some(token) { let _ = self.cancel(token); }
                None
            }
            ExecutorFact::DirectoryMoveReplaceRenameCleanupDisposition { token, item, intent } => {
                if item == super::model::ItemId(0) && self.running == Some(token)
                    && self.pending_directory_move_replace_rename_cleanup.get(&token).is_some_and(|(current, barrier)| current == &intent && *barrier == RenameCleanupBarrier::IntentDurable)
                {
                    if let Some((_, barrier)) = self.pending_directory_move_replace_rename_cleanup.get_mut(&token) { *barrier = RenameCleanupBarrier::ReceiptPending; }
                } else if self.running == Some(token) { let _ = self.cancel(token); }
                None
            }
            ExecutorFact::DirectoryMoveReplaceRenameBackupComplete { token, item } => {
                if item == super::model::ItemId(0) && self.running == Some(token)
                    && self.directory_move_replace_rename.get(&token).is_some_and(|ledger| ledger.phase == super::journal::DirectoryMoveReplaceRenamePhase::BackupCleanup && ledger.pending_cleanup.is_none() && ledger.backup_root_disposed && ledger.approved_target_children.iter().all(|child| child.disposition_recorded))
                    && self.pending_directory_move_replace_rename_cleanup.get(&token).is_some_and(|(_, barrier)| *barrier == RenameCleanupBarrier::Ready)
                {
                    if let Some((_, barrier)) = self.pending_directory_move_replace_rename_cleanup.get_mut(&token) { *barrier = RenameCleanupBarrier::BackupCompletePending; }
                } else if self.running == Some(token) { let _ = self.cancel(token); }
                None
            }
            ExecutorFact::CrossVolumeMoveReplacePhaseIntent { token, ledger } => {
                let valid_owner = self.running == Some(token)
                    && self.service.job(token.job_id()).is_some_and(|job| {
                        job.request.kind == super::model::OperationKind::Move
                            && job.request.sources.len() == 1
                            && super::journal::JournalPath::from_path(&job.request.sources[0])
                                == ledger.source
                            && ledger.item == 0
                    });
                let valid_phase = self.cross_volume_move_replaces.get(&token).map_or(
                    matches!(
                        ledger.phase,
                        super::journal::CrossVolumeMoveReplacePhase::StageReady
                            | super::journal::CrossVolumeMoveReplacePhase::StageCreateIntent
                    ),
                    |previous| {
                        super::journal::cross_volume_move_replace_same_identity(previous, &ledger)
                            && super::journal::cross_volume_phase_next(previous.phase, ledger.phase)
                    },
                );
                let backup_matches = self.replace_backups.get(&token).is_some_and(|backup| {
                    backup.source == ledger.source
                        && backup.destination == ledger.destination
                        && backup.backup == ledger.backup
                }) || (ledger.phase
                    == super::journal::CrossVolumeMoveReplacePhase::StageCreateIntent
                    && self.replace_backups.get(&token).is_none());
                if valid_owner && valid_phase && backup_matches {
                    self.pending_cross_volume_move_replace.insert(token, ledger);
                }
                None
            }
            ExecutorFact::RenameStageIntent {
                token,
                item,
                source,
                stage,
                destination,
                source_identity,
                parent_identity,
                phase,
            } => {
                if self.running == Some(token) {
                    let intent = super::journal::JournalRenameStage {
                        source: super::journal::JournalPath::from_path(&source),
                        stage: super::journal::JournalPath::from_path(&stage),
                        destination: super::journal::JournalPath::from_path(&destination),
                        source_identity,
                        parent_identity,
                        phase,
                    };
                    self.pending_rename_intents.insert(
                        token,
                        (
                            item,
                            source,
                            stage,
                            destination,
                            intent.source_identity.clone(),
                            intent.parent_identity.clone(),
                            phase,
                        ),
                    );
                    self.rename_stages.insert(token, intent);
                }
                None
            }
            ExecutorFact::ZipCreateLedgerIntent { token, ledger } => {
                let request_matches =
                    self.job(token).is_some_and(|job| {
                        job.request.kind == super::model::OperationKind::CreateZip
                            && job.request.sources.len() == ledger.sources.len()
                            && job.request.sources.iter().zip(&ledger.sources).all(
                                |(path, source)| {
                                    super::journal::JournalPath::from_path(path) == source.source
                                },
                            )
                            && job.request.destination.as_ref().is_some_and(|path| {
                                super::journal::JournalPath::from_path(path) == ledger.output
                            })
                            && job.request.zip_create_guard.as_ref().is_some_and(
                                |guard| match guard {
                                    super::model::ZipOperationGuard::Create(guard) => {
                                        guard.sources.len() == ledger.sources.len()
                                            && guard.sources.iter().zip(&ledger.sources).all(
                                                |(captured, source)| {
                                                    format!("{:?}", captured.source_identity)
                                                        == source.source_identity
                                                },
                                            )
                                            && format!("{:?}", guard.destination_parent_identity)
                                                == ledger.parent_identity
                                    }
                                    super::model::ZipOperationGuard::Extract(_)
                                    | super::model::ZipOperationGuard::ExtractMany(_) => false,
                                },
                            )
                    });
                let phase_matches = match self.zip_creates.get(&token) {
                    None => {
                        ledger.phase == super::journal::JournalZipCreatePhase::StagePlanned
                            && ledger.stage_identity.is_none()
                            && ledger.output_identity.is_none()
                    }
                    Some(previous) => {
                        super::journal::zip_create_transition_is_valid(previous, &ledger)
                    }
                };
                if self.running == Some(token)
                    && (self
                        .cancellations
                        .get(&token)
                        .is_some_and(|flag| !flag.load(Ordering::Acquire))
                        || ledger.phase == super::journal::JournalZipCreatePhase::StageDiscarded)
                    && request_matches
                    && phase_matches
                {
                    self.zip_creates.insert(token, ledger.clone());
                    self.pending_zip_create_intents.insert(token, ledger);
                }
                None
            }
            ExecutorFact::SevenZipCreateLedgerIntent { token, ledger } => {
                let request_matches = self.job(token).is_some_and(|job| {
                    let Some(guard) = job.request.seven_zip_create_guard.as_ref() else {
                        return false;
                    };
                    let Some(destination) = job.request.destination.as_ref() else {
                        return false;
                    };
                    job.request.kind == super::model::OperationKind::Create7z
                        && super::journal::JournalPath::from_path(destination) == ledger.output
                        && ledger.output_parent_identity
                            == format!("{:?}", guard.destination_parent_identity)
                        && ledger.provider_revision == guard.provider_revision
                        && ledger.executable
                            == super::journal::JournalPath::from_path(
                                &guard.provider_binding.executable.path,
                            )
                        && ledger.executable_identity
                            == format!("{:?}", guard.provider_binding.executable.identity)
                        && ledger.seven_zip_dll
                            == super::journal::JournalPath::from_path(
                                &guard.provider_binding.zip_dll.path,
                            )
                        && ledger.seven_zip_dll_identity
                            == format!("{:?}", guard.provider_binding.zip_dll.identity)
                        && job.request.sources.len() == ledger.sources.len()
                        && job
                            .request
                            .sources
                            .iter()
                            .zip(&guard.sources)
                            .zip(&ledger.sources)
                            .enumerate()
                            .all(|(index, ((path, expected), source))| {
                                let identity = expected.source_identity.windows_parts();
                                let associated = ledger
                                    .manifest
                                    .iter()
                                    .filter(|entry| {
                                        entry.source_index == Some(index as u32)
                                            && !entry.name.contains('/')
                                    })
                                    .collect::<Vec<_>>();
                                source.path == super::journal::JournalPath::from_path(path)
                                    && identity.is_some_and(|(volume, file_index)| {
                                        source.identity.volume == volume
                                            && source.identity.file_index == file_index
                                    })
                                    && associated.len() == 1
                                    && associated[0].name == expected.entry_name
                                    && associated[0].directory
                                        == (expected.source_kind
                                            == crate::directory::EntryKind::Folder)
                            })
                });
                let previous = self.seven_zip_creates.get(&token);
                let phase_valid = match previous {
                    None => {
                        ledger.phase
                            == super::journal::JournalSevenZipHandlePhase::CandidateCreateIntent
                            && ledger.sequence == 1
                            && !ledger.aborting
                    }
                    Some(previous) => ledger.follows(previous),
                };
                if self.running == Some(token)
                    && request_matches
                    && phase_valid
                    && ledger.validate().is_ok()
                    && !self.pending_seven_zip_create_intents.contains_key(&token)
                    && !self.failed_seven_zip_create_journal.contains_key(&token)
                {
                    self.pending_seven_zip_create_intents.insert(token, ledger);
                }
                None
            }
            ExecutorFact::ZipExtractLedgerIntent {
                token,
                item,
                ledger,
            } => {
                let request_matches = self.job(token).is_some_and(|job| {
                    job.request.kind == super::model::OperationKind::ExtractZip
                        && job
                            .request
                            .sources
                            .get(item.0 as usize)
                            .is_some_and(|path| {
                                super::journal::JournalPath::from_path(path) == ledger.source
                            })
                        && job
                            .request
                            .sources
                            .get(item.0 as usize)
                            .and_then(|source| {
                                source.parent().map(|parent| {
                                    parent.join(source.file_stem().unwrap_or_default())
                                })
                            })
                            .is_some_and(|path| {
                                super::journal::JournalPath::from_path(&path) == ledger.output
                            })
                        && job
                            .request
                            .zip_create_guard
                            .as_ref()
                            .is_some_and(|guard| match guard {
                                super::model::ZipOperationGuard::Extract(guard) => {
                                    item == super::model::ItemId(0)
                                        && format!("{:?}", guard.source_identity)
                                            == ledger.source_identity
                                        && format!("{:?}", guard.destination_parent_identity)
                                            == ledger.parent_identity
                                }
                                super::model::ZipOperationGuard::ExtractMany(guard) => guard
                                    .sources
                                    .get(item.0 as usize)
                                    .is_some_and(|source_guard| {
                                        format!("{:?}", source_guard.source_identity)
                                            == ledger.source_identity
                                            && format!("{:?}", guard.destination_parent_identity)
                                                == ledger.parent_identity
                                    }),
                                super::model::ZipOperationGuard::Create(_) => false,
                            })
                });
                let phase_matches = match self.zip_extracts.get(&(token, item)) {
                    None => {
                        ledger.phase == super::journal::JournalZipExtractPhase::StagePlanned
                            && ledger.stage_identity.is_none()
                    }
                    Some(previous) => {
                        super::journal::zip_extract_transition_is_valid(previous, &ledger)
                    }
                };
                if self.running == Some(token)
                    && (self
                        .cancellations
                        .get(&token)
                        .is_some_and(|flag| !flag.load(Ordering::Acquire))
                        || ledger.phase == super::journal::JournalZipExtractPhase::StageDiscarded
                        || (ledger.phase == super::journal::JournalZipExtractPhase::StagePlanned
                            && ledger.stage_identity.is_none()
                            && !self.zip_extracts.contains_key(&(token, item))))
                    && request_matches
                    && phase_matches
                {
                    self.zip_extracts.insert((token, item), ledger.clone());
                    self.pending_zip_extract_intents
                        .insert((token, item), ledger);
                }
                None
            }
            ExecutorFact::SevenZipExtractBatchIntent { token, batch } => {
                let request_matches = self.job(token).is_some_and(|job| {
                    let Some(guard) = job.request.seven_zip_extract_guard.as_ref() else {
                        return false;
                    };
                    let expected_attempt =
                        self.run_id
                            .map(|run_id| super::journal::JournalSevenZipExtractAttemptId {
                                run_id,
                                job: token.job_id().value(),
                                attempt: token.attempt_id().value(),
                            });
                    expected_attempt.as_ref() == Some(&batch.envelope.attempt)
                        && job.request.kind == super::model::OperationKind::Extract7z
                        && job.request.sources.len() == guard.sources.len()
                        && batch.envelope.provider_revision == guard.provider_revision
                        && batch.envelope.provider_executable.path
                            == super::journal::JournalPath::from_path(
                                &guard.provider_binding.executable.path,
                            )
                        && batch.envelope.provider_executable.identity.volume
                            == guard.provider_binding.executable.identity.volume_serial
                        && batch.envelope.provider_executable.identity.file_index
                            == guard.provider_binding.executable.identity.file_index
                        && batch.envelope.provider_dll.path
                            == super::journal::JournalPath::from_path(
                                &guard.provider_binding.zip_dll.path,
                            )
                        && batch.envelope.provider_dll.identity.volume
                            == guard.provider_binding.zip_dll.identity.volume_serial
                        && batch.envelope.provider_dll.identity.file_index
                            == guard.provider_binding.zip_dll.identity.file_index
                        && guard
                            .destination_parent_identity
                            .windows_parts()
                            .is_some_and(|(volume, file_index)| {
                                batch.envelope.destination_parent.volume == volume
                                    && batch.envelope.destination_parent.file_index == file_index
                            })
                        && job
                            .request
                            .sources
                            .iter()
                            .zip(&guard.sources)
                            .zip(&batch.envelope.items)
                            .enumerate()
                            .all(|(ordinal, ((source, source_guard), descriptor))| {
                                source_guard.source_identity.windows_parts().is_some_and(
                                    |(volume, file_index)| {
                                        descriptor.ordinal == ordinal as u32
                                            && descriptor.source_path
                                                == super::journal::JournalPath::from_path(source)
                                            && descriptor.source_identity.volume == volume
                                            && descriptor.source_identity.file_index == file_index
                                            && source.parent().is_some_and(|parent| {
                                                descriptor.output
                                                    == super::journal::JournalPath::from_path(
                                                        &parent.join(
                                                            source.file_stem().unwrap_or_default(),
                                                        ),
                                                    )
                                            })
                                    },
                                )
                            })
                });
                let terminal = self.job(token).is_some_and(|job| {
                    job.attempts.iter().any(|attempt| {
                        attempt.token == token
                            && attempt.state == super::model::AttemptState::Terminal
                    })
                });
                let state = self.seven_zip_extract_batches.entry(token).or_default();
                let previous = state
                    .pending
                    .as_ref()
                    .or(state.failed.as_ref())
                    .or(state.durable.as_ref());
                let progression = match previous {
                    None => batch.is_registration(),
                    Some(previous) => batch.follows(previous),
                };
                let observation_successor = state.journal_failed
                    && state
                        .failed
                        .as_ref()
                        .is_some_and(|failed| batch.is_observation_successor_of(failed));
                let accepted = self.running == Some(token)
                    && request_matches
                    && batch.validate().is_ok()
                    && progression
                    && state.pending.is_none()
                    && (!state.journal_failed || observation_successor)
                    && !terminal;
                if accepted {
                    state.pending = Some(batch);
                }
                if accepted {
                    state.pending_write_claimed = false;
                    let snapshot = self.journal_attempt(token);
                    if let Some(state) = self.seven_zip_extract_batches.get_mut(&token) {
                        state.pending_record = snapshot;
                    }
                }
                None
            }
            ExecutorFact::BatchRenameItemCompleted {
                token,
                item,
                source,
                destination,
                ..
            } => {
                let target = self.service.job(token.job_id()).and_then(|job| {
                    job.request
                        .batch_rename_plan
                        .and_then(|plan| plan.get(item.0 as usize).cloned())
                });
                if self.running == Some(token)
                    && target.as_ref().is_some_and(|target| {
                        target.source == source && target.destination == destination
                    })
                {
                    if let Some(target) = target {
                        let receipt = super::journal::JournalBatchRenameReceipt {
                            source: super::journal::JournalPath::from_path(&source),
                            destination: super::journal::JournalPath::from_path(&destination),
                            source_identity: format!("{:?}", target.source_identity),
                            parent_identity: format!("{:?}", target.parent_identity),
                            source_kind: Some(match target.kind {
                                crate::directory::EntryKind::File => super::journal::JournalBatchRenameKind::File,
                                crate::directory::EntryKind::Folder => super::journal::JournalBatchRenameKind::Folder,
                            }),
                            reparse_tag: target.reparse_guard.as_ref().map(|guard| guard.reparse_tag),
                        };
                        let completed = self.batch_rename_completed.entry(token).or_default();
                        if completed.last() != Some(&receipt) {
                            completed.push(receipt);
                        }
                        self.pending_batch_rename_items
                            .insert(token, (item, source, destination));
                    }
                }
                None
            }
            ExecutorFact::Conflict { token, notice } => {
                self.service.conflict(token, notice);
                None
            }
            ExecutorFact::Progress { token, update } => {
                self.commit_progress(token, &update);
                None
            }
            ExecutorFact::CreatedObjectUndoCandidate { token, item, receipt } => {
                let key = (token, item);
                let valid = self.running == Some(token)
                    && self.service.job(token.job_id()).is_some_and(|job| {
                        let expected_parent_identity = match job.request.kind {
                            super::model::OperationKind::CreateShortcut => job
                                .request
                                .shortcut_create_guard
                                .as_ref()
                                .and_then(|guard| guard.parent_identity.windows_parts()),
                            super::model::OperationKind::CreateFile
                            | super::model::OperationKind::CreateFolder => job
                                .request
                                .create_parent_directory_identity_guard
                                .as_ref()
                                .and_then(crate::filesystem_identity::FilesystemIdentity::windows_parts),
                            _ => None,
                        };
                        let parent_identity_matches = expected_parent_identity.is_some_and(|(volume, index)| {
                            receipt.parent_identity
                                == format!("{}:{:08x}{:08x}", volume, (index >> 32) as u32, index as u32)
                        });
                        let kind_and_output_match = match job.request.kind {
                            super::model::OperationKind::CreateFile
                            | super::model::OperationKind::CreateFolder => {
                                job.request.destination.as_deref() == receipt.destination.parent()
                            }
                            super::model::OperationKind::CreateShortcut => {
                                let proof_is_bounded = match &receipt.kind {
                                    super::model::CreatedObjectUndoKind::ShortcutFile {
                                        content_sha256,
                                        signature,
                                        size,
                                    } => {
                                        *size > 0
                                            && *size <= 1024 * 1024
                                            && signature.number_of_links == 1
                                            && signature.change_time != 0
                                            && content_sha256.len() == 64
                                            && content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                                    }
                                    _ => false,
                                };
                                job.request.shortcut_create_shape_is_valid()
                                    && job.request.shortcut_output_path().as_ref() == Some(&receipt.destination)
                                    && proof_is_bounded
                            }
                            _ => false,
                        };
                        item == super::model::ItemId(0)
                            && kind_and_output_match
                            && parent_identity_matches
                            && job.attempts.last().is_some_and(|attempt| {
                                attempt.token == token
                                    && matches!(attempt.state, super::model::AttemptState::Running | super::model::AttemptState::CancelRequested)
                            })
                    });
                if valid && !self.invalid_created_object_undo_candidates.contains(&key) {
                    match self.created_object_undo_candidates.get(&key) {
                        Some(existing) if existing != &receipt => {
                            self.created_object_undo_candidates.remove(&key);
                            self.invalid_created_object_undo_candidates.insert(key);
                        }
                        Some(_) if self.service.job(token.job_id()).is_some_and(|job| {
                            job.request.kind == super::model::OperationKind::CreateShortcut
                        }) => {
                            self.created_object_undo_candidates.remove(&key);
                            self.invalid_created_object_undo_candidates.insert(key);
                        }
                        Some(_) => {}
                        None => { self.created_object_undo_candidates.insert(key, receipt); }
                    }
                }
                None
            }
            ExecutorFact::OrdinaryCopySourceKind { token, item, kind } => {
                let valid = self.running == Some(token) && self.service.job(token.job_id()).is_some_and(|job| {
                    job.request.kind == super::model::OperationKind::Copy
                        && job.request.virtual_source.is_none()
                        && (item.0 as usize) < job.request.sources.len()
                        && job.attempts.last().is_some_and(|attempt| attempt.token == token
                            && attempt.retry_items.is_empty()
                            && matches!(attempt.state, super::model::AttemptState::Running | super::model::AttemptState::CancelRequested))
                });
                if valid { self.ordinary_copy_source_kinds.insert((token, item), kind); }
                None
            }
            ExecutorFact::DirectoryCopyUndoCandidate { token, item, receipt } => {
                let key = (token, item);
                let route_valid = receipt.destination_parent_route.last().is_some_and(|parent| {
                        Some(parent.path.as_path()) == receipt.destination.parent()
                            && parent.identity.windows_parts().is_some()
                    })
                    && receipt.root_identity.len() <= 64
                    && receipt.root_identity.contains(':');
                let valid = self.running == Some(token)
                    && self.service.job(token.job_id()).is_some_and(|job| {
                        job.request.kind == super::model::OperationKind::Copy
                            && !job.request.sources.is_empty()
                            && job.request.virtual_source.is_none()
                            && (item.0 as usize) < job.request.sources.len()
                            && route_valid
                            && job.request.destination.as_deref() == receipt.destination.parent()
                            && job.request.sources.get(item.0 as usize).is_some_and(|source| {
                                if source.file_name() == receipt.destination.file_name() {
                                    true
                                } else {
                                    let base = job.request.destination.as_ref().unwrap().join(source.file_name().unwrap_or_default());
                                    self.keep_both_directory_items.contains(&key)
                                        && super::model::is_keep_both_directory_candidate_path(&base, &receipt.destination)
                                }
                            })
                            && receipt.entries.len() <= 1_024
                            && receipt.entries.iter().all(|entry| {
                                !entry.relative_components.is_empty()
                                    && entry.relative_components.len() <= 128
                                    && !entry.identity.is_empty()
                                    && entry.identity.len() <= 64
                                    && match entry.kind {
                                        super::journal::JournalDirectoryNodeKind::Directory => entry.signature.is_none() && entry.content_sha256.is_none() && entry.length == 0,
                                        super::journal::JournalDirectoryNodeKind::File => entry.signature.is_some_and(|signature| signature.number_of_links == 1 && signature.change_time != 0) && entry.content_sha256.as_ref().is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())),
                                        super::journal::JournalDirectoryNodeKind::Unknown => false,
                                    }
                            })
                            && job.attempts.last().is_some_and(|attempt| attempt.token == token
                                && matches!(attempt.state, super::model::AttemptState::Running | super::model::AttemptState::CancelRequested))
                    });
                if valid && !self.invalid_directory_copy_undo_candidates.contains(&key) {
                    match self.directory_copy_undo_candidates.get(&key) {
                        Some(existing) if existing != &receipt => {
                            self.directory_copy_undo_candidates.remove(&key);
                            self.invalid_directory_copy_undo_candidates.insert(key);
                        }
                        Some(_) if self.service.job(token.job_id()).is_some_and(|job| {
                            job.request.copy_destination_identity_guard.is_some()
                        }) => {
                            self.directory_copy_undo_candidates.remove(&key);
                            self.invalid_directory_copy_undo_candidates.insert(key);
                        }
                        Some(_) => {}
                        None => { self.directory_copy_undo_candidates.insert(key, receipt); }
                    }
                }
                None
            }
            ExecutorFact::MoveUndoCandidate { token, item, receipt } => {
                let key = (token, item);
                let eligible_item = self.running == Some(token) && self.service.job(token.job_id()).is_some_and(|job| {
                    matches!(job.request.kind, super::model::OperationKind::Move | super::model::OperationKind::Rename)
                        && (item.0 as usize) < job.request.sources.len()
                        && job.attempts.last().is_some_and(|attempt| {
                            attempt.token == token
                                && matches!(attempt.state, super::model::AttemptState::Running | super::model::AttemptState::CancelRequested)
                        })
                });
                let valid = eligible_item && self.service.job(token.job_id()).is_some_and(|job| {
                    let index = item.0 as usize;
                    let clipboard_evidence_matches = job.request.clipboard_move_source_evidence.as_ref().is_none_or(|evidence| {
                        evidence.len() == job.request.sources.len()
                            && evidence.get(index).is_some_and(|accepted| {
                                let accepted_identity = receipt.cross_volume_file.as_ref()
                                    .map(|proof| &proof.original_source_identity)
                                    .unwrap_or(&receipt.object_identity);
                                let kind_matches = match receipt.object_kind {
                                    super::model::MoveUndoObjectKind::RegularFile => accepted.source_kind == crate::directory::EntryKind::File && accepted.reparse_tag.is_none(),
                                    super::model::MoveUndoObjectKind::Directory => accepted.source_kind == crate::directory::EntryKind::Folder && accepted.reparse_tag.is_none(),
                                    super::model::MoveUndoObjectKind::ReparsePoint { tag, is_directory } => accepted.reparse_tag == Some(tag)
                                        && matches!(tag, 0xA000_000C | 0xA000_0003)
                                        && (tag != 0xA000_0003 || is_directory)
                                        && accepted.source_kind == if is_directory { crate::directory::EntryKind::Folder } else { crate::directory::EntryKind::File },
                                    super::model::MoveUndoObjectKind::CaseOnlyRename => false,
                                };
                                accepted.source == receipt.source
                                    && accepted.source_identity == *accepted_identity
                                    && kind_matches
                            })
                    });
                    job.request.sources.get(index) == Some(&receipt.source)
                        && clipboard_evidence_matches
                        && (if job.request.kind == super::model::OperationKind::Rename {
                            job.request.destination.as_ref() == Some(&receipt.destination)
                        } else {
                            job.request.destination.as_deref() == receipt.destination.parent()
                        })
                        && match (&receipt.cross_volume_file, job.request.move_identity_guard.as_ref()) {
                            (None, Some(guard)) if job.request.kind == super::model::OperationKind::Rename => {
                                index == 0
                                    && job.request.sources.len() == 1
                                    && job.request.virtual_source.is_none()
                                    && job.request.rename_reparse_guard.is_none()
                                    && job.request.move_destination_identity_guard.is_none()
                                    && job.request.move_replace_source_evidence.is_none()
                                    && guard.source_identities.len() == 1
                                    && guard.source_identities[0] == receipt.object_identity
                                    && Some(&guard.destination_identity) == receipt.destination_parent_route.last().map(|parent| &parent.identity)
                                    && job.request.destination.as_ref() == Some(&receipt.destination)
                                    && match receipt.object_kind {
                                        super::model::MoveUndoObjectKind::RegularFile
                                        | super::model::MoveUndoObjectKind::Directory => true,
                                        super::model::MoveUndoObjectKind::CaseOnlyRename => receipt.destination.file_name()
                                            .and_then(|name| name.to_str())
                                            .is_some_and(|name| super::rename::is_case_only_rename(&receipt.source, name)),
                                        super::model::MoveUndoObjectKind::ReparsePoint { .. } => false,
                                    }
                                    && receipt.source != receipt.destination
                                    && (receipt.object_kind == super::model::MoveUndoObjectKind::CaseOnlyRename
                                        || !receipt.destination.file_name().and_then(|name| name.to_str()).is_some_and(|name| super::rename::is_case_only_rename(&receipt.source, name)))
                                    && receipt.source.parent() == receipt.destination.parent()
                                    && receipt.source_parent_route.last().is_some_and(|parent| Some(parent.path.as_path()) == receipt.source.parent())
                                    && receipt.destination_parent_route.last().is_some_and(|parent| Some(parent.path.as_path()) == receipt.destination.parent())
                                    && receipt.source_parent_route.last().is_some_and(|parent| &parent.identity == &guard.destination_identity)
                                    && receipt.destination_parent_route.last().is_some_and(|parent| &parent.identity == &guard.destination_identity)
                                    && receipt.cross_volume_file.is_none()
                                    && job.attempts.last().is_some_and(|attempt| attempt.retry_items.is_empty())
                            }
                            (None, None)
                                if job.request.kind == super::model::OperationKind::Move
                                    && job.request.move_replace_source_evidence.is_some() =>
                            {
                                index < job.request.sources.len()
                                    && job.request.virtual_source.is_none()
                                    && job.request.move_identity_guard.is_none()
                                    && job.request.move_destination_identity_guard.is_some()
                                    && job.request.move_replace_source_evidence.as_ref().is_some_and(|proofs| proofs.len() == job.request.sources.len())
                                    && job.attempts.last().is_some_and(|attempt| {
                                        attempt.token == token && attempt.retry_items.is_empty()
                                    })
                                    && receipt.object_kind
                                        == super::model::MoveUndoObjectKind::RegularFile
                                    && job.request.move_replace_source_evidence.as_ref()
                                        .and_then(|evidence| evidence.get(index))
                                        .is_some_and(|evidence| evidence.source_identity == receipt.object_identity)
                                    && (receipt.destination.file_name() == receipt.source.file_name()
                                        || job.request.sources.get(index).and_then(|source| source.file_name()).is_some_and(|leaf| {
                                            job.request.destination.as_ref().is_some_and(|parent| {
                                                super::model::is_keep_both_candidate_path(&parent.join(leaf), &receipt.destination)
                                            })
                                        }))
                                    && receipt.source_parent_route.last().is_some_and(|parent| {
                                        receipt.source.parent() == Some(parent.path.as_path())
                                    })
                                    && receipt.destination_parent_route.last().is_some_and(|parent| {
                                        receipt.destination.parent() == Some(parent.path.as_path())
                                            && job.request.destination.as_deref()
                                                == Some(parent.path.as_path())
                                            && job.request.move_destination_identity_guard.as_ref()
                                                == Some(&parent.identity)
                                    })
                                    && receipt.source_parent_route.last()
                                        .zip(receipt.destination_parent_route.last())
                                        .is_some_and(|(source, destination)| {
                                            source.identity.windows_parts().map(|(volume, _)| volume)
                                                == destination.identity.windows_parts().map(|(volume, _)| volume)
                                        })
                            }
                            (None, None)
                                if job.request.kind == super::model::OperationKind::Move
                                    && job.request.move_destination_identity_guard.is_some()
                                    && matches!(receipt.object_kind, super::model::MoveUndoObjectKind::ReparsePoint { .. }) =>
                            {
                                let destination_parent = receipt.destination_parent_route.last();
                                let source_parent = receipt.source_parent_route.last();
                                let exact_kind = match receipt.object_kind {
                                    super::model::MoveUndoObjectKind::ReparsePoint { tag, is_directory } => {
                                        matches!(tag, 0xA000_000C | 0xA000_0003)
                                            && (tag != 0xA000_0003 || is_directory)
                                    }
                                    _ => false,
                                };
                                index < job.request.sources.len()
                                    && job.request.virtual_source.is_none()
                                    && job.request.move_identity_guard.is_none()
                                    && job.request.move_replace_source_evidence.is_none()
                                    && job.request.rename_reparse_guard.is_none()
                                    && job.attempts.last().is_some_and(|attempt| attempt.token == token && attempt.retry_items.is_empty())
                                    && exact_kind
                                    && receipt.cross_volume_file.is_none()
                                    && receipt.source != receipt.destination
                                    && (receipt.source.file_name() == receipt.destination.file_name()
                                        || job.request.destination.as_ref().is_some_and(|parent| {
                                            super::model::is_keep_both_candidate_path(
                                                &parent.join(receipt.source.file_name().unwrap_or_default()),
                                                &receipt.destination,
                                            )
                                        }))
                                    && source_parent.is_some_and(|parent| receipt.source.parent() == Some(parent.path.as_path()))
                                    && destination_parent.is_some_and(|parent| {
                                        receipt.destination.parent() == Some(parent.path.as_path())
                                            && job.request.destination.as_deref() == Some(parent.path.as_path())
                                            && job.request.move_destination_identity_guard.as_ref() == Some(&parent.identity)
                                    })
                                    && source_parent.zip(destination_parent).is_some_and(|(source, destination)| {
                                        source.identity.windows_parts().zip(destination.identity.windows_parts())
                                            .is_some_and(|(source, destination)| source.0 == destination.0)
                                    })
                                    && receipt.source_parent_route.iter().all(|parent| parent.identity.windows_parts().is_some())
                                    && receipt.destination_parent_route.iter().all(|parent| parent.identity.windows_parts().is_some())
                            }
                            (None, None) if job.request.move_destination_identity_guard.is_some() => {
                                index < job.request.sources.len()
                                    && job.request.virtual_source.is_none()
                                    && job.request.move_identity_guard.is_none()
                                    && job.attempts.last().is_some_and(|attempt| attempt.token == token && attempt.retry_items.is_empty())
                                    && matches!(receipt.object_kind, super::model::MoveUndoObjectKind::RegularFile | super::model::MoveUndoObjectKind::Directory)
                                    && (receipt.source.file_name() == receipt.destination.file_name()
                                        || super::model::is_keep_both_candidate_path(
                                            &job.request.destination.as_ref().unwrap().join(
                                                receipt.source.file_name().unwrap_or_default(),
                                            ),
                                            &receipt.destination,
                                        ))
                                    && receipt.source_parent_route.last().is_some_and(|parent| receipt.source.parent() == Some(parent.path.as_path()))
                                    && receipt.destination_parent_route.last().is_some_and(|parent| {
                                        receipt.destination.parent() == Some(parent.path.as_path())
                                            && job.request.destination.as_deref() == Some(parent.path.as_path())
                                            && job.request.move_destination_identity_guard.as_ref() == Some(&parent.identity)
                                    })
                            }
                            (None, Some(guard)) => {
                                guard.source_identities.get(index) == Some(&receipt.object_identity)
                                    && Some(&guard.destination_identity) == receipt.destination_parent_route.last().map(|parent| &parent.identity)
                            }
                            (Some(proof), guard) if proof.directory_manifest.is_some() => {
                                let manifest = proof.directory_manifest.as_ref().unwrap();
                                let identity_text = |identity: &crate::filesystem_identity::FilesystemIdentity| {
                                    identity.windows_parts().map(|(volume, index)| format!("{volume}:{:08x}{:08x}", (index >> 32) as u32, index as u32))
                                };
                                let manifest_exact = manifest.item == item.0
                                    && manifest.source == super::journal::JournalPath::from_path(&receipt.source)
                                    && manifest.destination == super::journal::JournalPath::from_path(&receipt.destination)
                                    && manifest.cleanup_complete
                                    && manifest.pending.is_none()
                                    && manifest.replace.is_none()
                                    && manifest.root.as_ref().is_some_and(|root| {
                                        root.pending.is_none()
                                            && root.disposition_recorded
                                            && Some(root.source_identity.as_str()) == identity_text(&proof.original_source_identity).as_deref()
                                            && Some(root.destination_identity.as_str()) == identity_text(&receipt.object_identity).as_deref()
                                    })
                                    && manifest.children.iter().enumerate().all(|(ordinal, child)| {
                                        child.ordinal == ordinal as u32
                                            && child.disposition_recorded
                                            && child.node_kind != super::journal::JournalDirectoryNodeKind::Unknown
                                            && !child.source_identity.is_empty()
                                            && !child.destination_identity.is_empty()
                                    });
                                let clipboard_destination_guard_matches = job.request.move_destination_identity_guard.as_ref().is_some_and(|identity| {
                                    job.request.move_identity_guard.is_none()
                                        && receipt.destination_parent_route.last().is_some_and(|parent| {
                                            &parent.identity == identity
                                                && job.request.destination.as_deref() == Some(parent.path.as_path())
                                                && receipt.destination.parent() == Some(parent.path.as_path())
                                        })
                                });
                                index < job.request.sources.len()
                                    && job.attempts.last().is_some_and(|attempt| attempt.token == token && attempt.retry_items.is_empty())
                                    && job.request.virtual_source.is_none()
                                    && job.request.move_destination_identity_guard.as_ref().is_none_or(|_| clipboard_destination_guard_matches)
                                    && job.request.move_replace_source_evidence.is_none()
                                    && receipt.object_kind == super::model::MoveUndoObjectKind::Directory
                                    && (job.request.move_destination_identity_guard.is_none()
                                        || receipt.source.file_name() == receipt.destination.file_name()
                                        || super::model::is_keep_both_candidate_path(
                                            &job.request.destination.as_ref().unwrap().join(
                                                receipt.source.file_name().unwrap_or_default(),
                                            ),
                                            &receipt.destination,
                                        ))
                                    && guard.is_none_or(|guard| {
                                        guard.source_identities.get(index) == Some(&proof.original_source_identity)
                                            && Some(&guard.destination_identity) == receipt.destination_parent_route.last().map(|parent| &parent.identity)
                                    })
                                    && manifest_exact
                                    && proof.original_source_identity.windows_parts().zip(receipt.object_identity.windows_parts())
                                        .is_some_and(|(source, destination)| source.0 != destination.0)
                                    && receipt.source_parent_route.last().is_some_and(|parent| receipt.source.parent() == Some(parent.path.as_path()))
                                    && receipt.destination_parent_route.last().is_some_and(|parent| receipt.destination.parent() == Some(parent.path.as_path()))
                                    && receipt.source_parent_route.iter().all(|parent| parent.identity.windows_parts().is_some())
                                    && receipt.destination_parent_route.iter().all(|parent| parent.identity.windows_parts().is_some())
                            }
                            (Some(proof), guard) => {
                                let retry_guard_matches = self.service.cross_volume_retry_authority(token).map_or_else(
                                    || job.attempts.last().is_some_and(|attempt| attempt.token == token && attempt.retry_items.is_empty()),
                                    |(retry_items, retry_guards)| {
                                        retry_items.iter().filter(|retry_item| **retry_item == item).count() == 1
                                            && retry_guards.iter().filter(|retry_guard| retry_guard.proof.item == item).count() == 1
                                            && retry_guards.iter().find(|retry_guard| retry_guard.proof.item == item).is_some_and(|retry_guard| {
                                                let retry_proof = &retry_guard.proof;
                                                retry_proof.source == receipt.source
                                                    && retry_proof.destination == receipt.destination
                                                    && retry_proof.source_identity == proof.original_source_identity
                                                    && retry_proof.source_signature == proof.original_source_signature
                                                    && receipt.destination_parent_route.last().is_some_and(|parent| retry_proof.destination_parent_identity == parent.identity)
                                            })
                                    },
                                );
                                let clipboard_destination_guard_matches = job.request.move_identity_guard.is_none()
                                    && job.request.move_destination_identity_guard.as_ref().is_some_and(|identity| {
                                        receipt.destination_parent_route.last().is_some_and(|parent| {
                                            &parent.identity == identity
                                                && job.request.destination.as_deref() == Some(parent.path.as_path())
                                                && receipt.destination.parent() == Some(parent.path.as_path())
                                        })
                                    });
                                index < job.request.sources.len()
                                    && job.request.virtual_source.is_none()
                                    && (job.request.move_destination_identity_guard.is_none() || clipboard_destination_guard_matches)
                                    && job.request.move_replace_source_evidence.is_none()
                                    && retry_guard_matches
                                    && proof.directory_manifest.is_none()
                                    && (receipt.source.file_name() == receipt.destination.file_name()
                                        || (job.request.move_destination_identity_guard.is_some()
                                            && super::model::is_keep_both_candidate_path(
                                                &job.request.destination.as_ref().unwrap().join(
                                                    receipt.source.file_name().unwrap_or_default(),
                                                ),
                                                &receipt.destination,
                                            )))
                                    && receipt.object_kind == super::model::MoveUndoObjectKind::RegularFile
                                    && guard.is_none_or(|guard| {
                                        guard.source_identities.get(index) == Some(&proof.original_source_identity)
                                            && Some(&guard.destination_identity) == receipt.destination_parent_route.last().map(|parent| &parent.identity)
                                    })
                                    && proof.original_source_identity != receipt.object_identity
                                    && proof.original_source_signature.is_reliable_for_cross_volume_move_retry()
                                    && proof.destination_signature.is_reliable_for_cross_volume_move_retry()
                                    && proof.content_sha256.len() == 64
                                    && proof.content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                                    && proof.original_source_identity.windows_parts().zip(receipt.object_identity.windows_parts())
                                        .is_some_and(|(source, destination)| source.0 != destination.0)
                                    && (job.request.move_destination_identity_guard.is_none() || clipboard_destination_guard_matches)
                                    && receipt.source_parent_route.last().is_some_and(|parent| receipt.source.parent() == Some(parent.path.as_path()))
                                    && receipt.destination_parent_route.last().is_some_and(|parent| receipt.destination.parent() == Some(parent.path.as_path()))
                            }
                            _ => false,
                        }
                        && !receipt.source_parent_route.is_empty() && !receipt.destination_parent_route.is_empty()
                });
                if eligible_item && !self.invalid_move_undo_candidates.contains(&key) {
                    match self.move_undo_candidates.get(&key) {
                        Some(_) => {
                            self.move_undo_candidates.remove(&key);
                            self.invalid_move_undo_candidates.insert(key);
                        }
                        None if valid => { self.move_undo_candidates.insert(key, receipt); }
                        None => { self.invalid_move_undo_candidates.insert(key); }
                    }
                }
                None
            }
            ExecutorFact::Terminal { token, mut report } => {
                let is_seven_zip = self
                    .service
                    .job(token.job_id())
                    .is_some_and(|job| job.request.kind == super::model::OperationKind::Create7z);
                let durable_seven_zip_publication = is_seven_zip
                    && self.seven_zip_creates.get(&token).is_some_and(|ledger| {
                        ledger.phase == super::journal::JournalSevenZipHandlePhase::Published
                    })
                    && !self.pending_seven_zip_create_intents.contains_key(&token)
                    && !self.failed_seven_zip_create_journal.contains_key(&token);
                if is_seven_zip && report.outcome != TerminalOutcome::Succeeded {
                    let admitted_ledger = self.seven_zip_creates.get(&token);
                    let pending_or_failed =
                        self.pending_seven_zip_create_intents.contains_key(&token)
                            || self.failed_seven_zip_create_journal.contains_key(&token);
                    let durably_discarded = admitted_ledger.is_some_and(|ledger| {
                        ledger.phase == super::journal::JournalSevenZipHandlePhase::Discarded
                    }) && !pending_or_failed;
                    if (admitted_ledger.is_some() || pending_or_failed) && !durably_discarded {
                        let published = admitted_ledger.is_some_and(|ledger| {
                            ledger.phase == super::journal::JournalSevenZipHandlePhase::Published
                        });
                        report.retry = super::model::RetryPlan::RequiresReinspection;
                        if published {
                            // 公開済み出力は消さず、失敗報告より既知の成果物を優先する。
                            let output = self
                                .service
                                .job(token.job_id())
                                .and_then(|job| job.request.destination.clone())
                                .expect("Create7z request has a destination");
                            report.outcome = TerminalOutcome::Partial;
                            report.final_counters.processed_items = 1;
                            report.final_counters.total_items = Some(1);
                            report.receipts = vec![super::model::ItemReceipt {
                                item: super::model::ItemId(0),
                                outcome: super::model::ItemOutcome::Partial,
                                detail: Some(
                                    "archive was durably published before terminal failure".into(),
                                ),
                                created_folder_path: None,
                                created_archive_path: Some(output),
                                move_receipt: None,
                                replace_receipt: None,
                                copy_undo: None,
                            }];
                        } else {
                            if matches!(
                                report.outcome,
                                TerminalOutcome::Failed | TerminalOutcome::Cancelled
                            ) {
                                report.outcome = TerminalOutcome::Unknown;
                            }
                            report.final_counters.processed_items = 0;
                            report.final_counters.processed_bytes = 0;
                            for receipt in &mut report.receipts {
                                receipt.outcome = if report.outcome == TerminalOutcome::Partial {
                                    super::model::ItemOutcome::Partial
                                } else {
                                    super::model::ItemOutcome::Unknown
                                };
                                receipt.created_archive_path = None;
                            }
                        }
                    } else if durably_discarded {
                        report.final_counters.processed_items = 0;
                        report.final_counters.processed_bytes = 0;
                        for receipt in &mut report.receipts {
                            receipt.outcome = match report.outcome {
                                TerminalOutcome::Failed => super::model::ItemOutcome::Failed,
                                TerminalOutcome::Cancelled => {
                                    super::model::ItemOutcome::CancelledBeforeStart
                                }
                                TerminalOutcome::Unknown => super::model::ItemOutcome::Unknown,
                                TerminalOutcome::Partial => super::model::ItemOutcome::Partial,
                                TerminalOutcome::Succeeded => unreachable!(),
                            };
                            receipt.created_archive_path = None;
                        }
                    }
                }
                if report.outcome == TerminalOutcome::Succeeded && is_seven_zip {
                    if !durable_seven_zip_publication {
                        report.outcome = TerminalOutcome::Unknown;
                        report.retry = super::model::RetryPlan::RequiresReinspection;
                        report.final_counters.processed_items = 0;
                        report.final_counters.processed_bytes = 0;
                        for receipt in &mut report.receipts {
                            if receipt.outcome == super::model::ItemOutcome::Completed {
                                receipt.outcome = super::model::ItemOutcome::Unknown;
                                receipt.created_archive_path = None;
                            }
                        }
                    } else {
                        let published_output = self
                            .service
                            .job(token.job_id())
                            .and_then(|job| job.request.destination.clone())
                            .expect("Create7z request has a destination");
                        let report_matches_publication = report.receipts.len() == 1
                            && report.receipts.first().is_some_and(|receipt| {
                                receipt.item == super::model::ItemId(0)
                                    && receipt.outcome == super::model::ItemOutcome::Completed
                                    && receipt.created_archive_path.as_ref()
                                        == Some(&published_output)
                                    && receipt.created_folder_path.is_none()
                                    && receipt.move_receipt.is_none()
                                    && receipt.replace_receipt.is_none()
                            })
                            && report.final_counters.processed_items == 1
                            && report.final_counters.total_items == Some(1);
                        if !report_matches_publication {
                            // 公開済み archive は巻き戻せないため、既知の出力を Partial receipt に残す。
                            report.outcome = TerminalOutcome::Partial;
                            report.retry = super::model::RetryPlan::RequiresReinspection;
                            report.final_counters.processed_items = 1;
                            report.final_counters.total_items = Some(1);
                            report.receipts = vec![super::model::ItemReceipt {
                                item: super::model::ItemId(0),
                                outcome: super::model::ItemOutcome::Partial,
                                detail: Some(
                                    "published archive has an inconsistent terminal receipt".into(),
                                ),
                                created_folder_path: None,
                                created_archive_path: Some(published_output),
                                move_receipt: None,
                                replace_receipt: None,
                                copy_undo: None,
                            }];
                        }
                    }
                }
                let is_extract_7z = self
                    .service
                    .job(token.job_id())
                    .is_some_and(|job| job.request.kind == super::model::OperationKind::Extract7z);
                if is_extract_7z {
                    let source_count = self
                        .service
                        .job(token.job_id())
                        .map_or(0, |job| job.request.sources.len());
                    let state = self.seven_zip_extract_batches.get(&token);
                    let unresolved = state.is_some_and(|state| {
                        state.pending.is_some() || state.failed.is_some() || state.journal_failed
                    });
                    let batch = state.and_then(|state| {
                        state
                            .pending
                            .as_ref()
                            .or(state.failed.as_ref())
                            .or(state.durable.as_ref())
                    });
                    let mut normalized = Vec::with_capacity(source_count);
                    let mut exact_success = source_count > 0
                        && report.receipts.len() == source_count
                        && report.final_counters.processed_items == source_count as u64
                        && report.final_counters.total_items == Some(source_count as u64);
                    for ordinal in 0..source_count {
                        let item = super::model::ItemId(ordinal as u32);
                        let receipt = report.receipts.iter().find(|receipt| receipt.item == item);
                        let ledger = batch.and_then(|batch| {
                            batch
                                .items
                                .iter()
                                .find(|ledger| ledger.binding.item == ordinal as u32)
                        });
                        let published = ledger.filter(|ledger| {
                            ledger.phase == super::journal::JournalSevenZipExtractPhase::Published
                                && ledger.tree_state
                                    == super::journal::JournalSevenZipExtractObjectState::Published
                                && ledger.publication_identity.is_some()
                                && ledger.publication_identity == ledger.tree_identity
                        });
                        let discarded = ledger.is_some_and(|ledger| {
                            ledger.phase == super::journal::JournalSevenZipExtractPhase::Discarded
                                && matches!(ledger.spool_state, super::journal::JournalSevenZipExtractObjectState::Deleted
                                    | super::journal::JournalSevenZipExtractObjectState::CreateFailed
                                    | super::journal::JournalSevenZipExtractObjectState::NeverCreated)
                                && matches!(ledger.tree_state, super::journal::JournalSevenZipExtractObjectState::Deleted
                                    | super::journal::JournalSevenZipExtractObjectState::CreateFailed
                                    | super::journal::JournalSevenZipExtractObjectState::NeverCreated)
                        });
                        let known_output = ledger
                            .filter(|ledger| ledger.publication_identity.is_some())
                            .map(|ledger| ledger.binding.output.clone());
                        let mut normalized_receipt =
                            receipt.cloned().unwrap_or(super::model::ItemReceipt {
                                item,
                                outcome: super::model::ItemOutcome::Unknown,
                                detail: Some("missing extraction terminal receipt".into()),
                                created_folder_path: None,
                                created_archive_path: None,
                                move_receipt: None,
                                replace_receipt: None,
                                copy_undo: None,
                            });
                        let valid_completed = receipt.is_some_and(|receipt| {
                            receipt.outcome == super::model::ItemOutcome::Completed
                                && receipt.created_folder_path.as_ref()
                                    == published
                                        .and_then(|ledger| ledger.binding.output.to_path())
                                        .as_ref()
                                && receipt.created_archive_path.is_none()
                                && receipt.move_receipt.is_none()
                                && receipt.replace_receipt.is_none()
                        }) && published.is_some();
                        exact_success &= valid_completed;
                        if valid_completed {
                            normalized_receipt.created_folder_path =
                                known_output.and_then(|path| path.to_path());
                        } else if published.is_some() {
                            normalized_receipt.outcome = super::model::ItemOutcome::Partial;
                            normalized_receipt.created_folder_path =
                                known_output.and_then(|path| path.to_path());
                            normalized_receipt.created_archive_path = None;
                        } else if unresolved || (ledger.is_some() && !discarded) {
                            normalized_receipt.outcome =
                                if report.outcome == TerminalOutcome::Partial {
                                    super::model::ItemOutcome::Partial
                                } else {
                                    super::model::ItemOutcome::Unknown
                                };
                            normalized_receipt.created_folder_path =
                                known_output.and_then(|path| path.to_path());
                            normalized_receipt.created_archive_path = None;
                        } else if discarded || ledger.is_none() {
                            normalized_receipt.created_folder_path = None;
                            normalized_receipt.created_archive_path = None;
                            normalized_receipt.outcome = match report.outcome {
                                TerminalOutcome::Failed => super::model::ItemOutcome::Failed,
                                TerminalOutcome::Cancelled => {
                                    super::model::ItemOutcome::CancelledBeforeStart
                                }
                                TerminalOutcome::Partial => super::model::ItemOutcome::Partial,
                                TerminalOutcome::Unknown => super::model::ItemOutcome::Unknown,
                                TerminalOutcome::Succeeded => super::model::ItemOutcome::Unknown,
                            };
                        }
                        normalized.push(normalized_receipt);
                    }
                    report.receipts = normalized;
                    let completed = report
                        .receipts
                        .iter()
                        .filter(|receipt| receipt.outcome == super::model::ItemOutcome::Completed)
                        .count();
                    report.final_counters.total_items = Some(source_count as u64);
                    report.final_counters.processed_items = completed as u64;
                    if report.outcome == TerminalOutcome::Succeeded && !exact_success {
                        report.outcome = if completed > 0 {
                            TerminalOutcome::Partial
                        } else {
                            TerminalOutcome::Unknown
                        };
                    } else if report.outcome != TerminalOutcome::Succeeded && completed > 0 {
                        report.outcome = TerminalOutcome::Partial;
                    }
                    if unresolved
                        || report.receipts.iter().any(|receipt| {
                            matches!(
                                receipt.outcome,
                                super::model::ItemOutcome::Partial
                                    | super::model::ItemOutcome::Unknown
                            )
                        })
                    {
                        report.retry = super::model::RetryPlan::RequiresReinspection;
                        if report.outcome == TerminalOutcome::Failed
                            || report.outcome == TerminalOutcome::Cancelled
                        {
                            report.outcome = TerminalOutcome::Unknown;
                        }
                    }
                }
                let cross_volume_replace_incomplete = self
                    .cross_volume_move_replaces
                    .get(&token)
                    .is_some_and(|ledger| {
                        ledger.phase != super::journal::CrossVolumeMoveReplacePhase::Complete
                    })
                    || self.pending_cross_volume_move_replace.contains_key(&token);
                let directory_move_replace_rename_incomplete = self
                    .directory_move_replace_rename
                    .get(&token)
                    .is_some_and(|ledger| {
                        ledger.phase != super::journal::DirectoryMoveReplaceRenamePhase::Complete
                            && ledger.phase != super::journal::DirectoryMoveReplaceRenamePhase::NoEffect
                    })
                    || self.pending_directory_move_replace_rename.contains_key(&token)
                    || self.pending_directory_move_replace_rename_cleanup.get(&token).is_some_and(|(_, barrier)| *barrier != RenameCleanupBarrier::Ready);
                if report.outcome == TerminalOutcome::Succeeded
                    && (self.directory_moves.iter().any(|((attempt, _), state)| {
                        *attempt == token
                            && !matches!(state.barrier, DirectoryMoveBarrier::Complete)
                    }) || self
                        .cancellations
                        .get(&token)
                        .is_none_or(|flag| flag.load(Ordering::Acquire))
                        && cross_volume_replace_incomplete
                        || directory_move_replace_rename_incomplete)
                {
                    report.outcome = TerminalOutcome::Partial;
                    report.retry = super::model::RetryPlan::RequiresReinspection;
                }
                let synthesized_unknown = report.outcome == TerminalOutcome::Unknown
                    && self.missing_terminal_candidates.remove(&token);
                let committed = self.service.terminal(token, report).map(|event| self.accept_terminal(event));
                if committed.is_none() {
                    self.created_object_undo_candidates.retain(|(candidate_token, _), _| *candidate_token != token);
                    self.invalid_created_object_undo_candidates.retain(|(candidate_token, _)| *candidate_token != token);
                    self.move_undo_candidates.retain(|(candidate_token, _), _| *candidate_token != token);
                    self.invalid_move_undo_candidates.retain(|(candidate_token, _)| *candidate_token != token);
                    self.directory_copy_undo_candidates.retain(|(candidate_token, _), _| *candidate_token != token);
                    self.invalid_directory_copy_undo_candidates.retain(|(candidate_token, _)| *candidate_token != token);
                    self.ordinary_copy_source_kinds.retain(|(candidate_token, _), _| *candidate_token != token);
                }
                if synthesized_unknown && committed.is_some() { self.synthesized_unknown_attempts.insert(token); }
                committed
            }
        }
    }

    fn accept_terminal(&mut self, event: ServiceEvent) -> TerminalCommit {
        let ServiceEvent::TerminalCommitted { token, report } = &event;
        if let Some(sources) = self
            .service
            .job(token.job_id())
            .and_then(|job| job.request.virtual_source)
        {
            for source in sources.items {
                super::virtual_transfer::release(source.token);
            }
        }
        // Never remove a retry's cancellation flag on a late terminal from the
        // preceding attempt.  `terminal` has already checked current identity.
        self.cancellations.remove(token);
        self.reject_copy_stage_cleanup_running_ack(*token);
        self.source_cleanup_leases.remove(token);
        // A queued cleanup can be cancelled before `start_next` transfers its
        // handle to a worker. Drop that exact capability on every terminal path.
        self.copy_stage_cleanup_leases.remove(token);
        if !self.is_seven_zip_extract_attempt(*token) {
            self.controls.remove(token);
        }
        self.pending_replace_intents.remove(token);
        self.pending_cross_volume_move_replace.remove(token);
        self.pending_directory_move_replace_rename.remove(token);
        // Keep the typed E7 ledger for terminal persistence and recovery, but
        // discard its worker-bound cleanup barrier once this attempt settles.
        self.pending_directory_move_replace_rename_cleanup.remove(token);
        self.directory_moves
            .retain(|(attempt, _), _| attempt != token);
        self.directory_move_replaces
            .retain(|(attempt, _), _| attempt != token);
        self.pending_rename_intents.remove(token);
        self.pending_zip_create_intents.remove(token);
        self.zip_creates.remove(token);
        // terminal journal を書き終えるまで、公開済みを含む正確な 7z ledger を保持する。
        self.pending_zip_extract_intents
            .retain(|(attempt, _), _| attempt != token);
        if !matches!(
            report.outcome,
            TerminalOutcome::Partial | TerminalOutcome::Unknown
        ) {
            if !self.cross_volume_move_replaces.contains_key(token) {
                self.replace_backups.remove(token);
            }
            self.rename_stages.remove(token);
        }
        let receipts = self.pending_trash.remove(token).unwrap_or_default();
        if !receipts.is_empty() && matches!(report.outcome, TerminalOutcome::Succeeded | TerminalOutcome::Partial) {
            let trash_job = self.service.job(token.job_id()).filter(|job| {
                job.request.kind == super::model::OperationKind::Trash && job.request.virtual_source.is_none()
            });
            let group = trash_job.as_ref().and_then(|job| {
                let sources = &job.request.sources;
                if sources.len() <= 1 {
                    return None;
                }
                if report.outcome == TerminalOutcome::Succeeded {
                    if report.receipts.len() != sources.len()
                        || receipts.len() != sources.len()
                        || report.receipts.iter().enumerate().any(|(index, receipt)| {
                            receipt.item != super::model::ItemId(index as u32)
                                || receipt.outcome != super::model::ItemOutcome::Completed
                        })
                    {
                        return None;
                    }
                    let mut ordered = Vec::with_capacity(sources.len());
                    for (index, source) in sources.iter().enumerate() {
                        let item = super::model::ItemId(index as u32);
                        let facts = receipts
                            .iter()
                            .filter(|(candidate, _)| *candidate == item)
                            .collect::<Vec<_>>();
                        if facts.len() != 1 || facts[0].1.original_path() != *source {
                            return None;
                        }
                        ordered.push(facts[0].1.clone());
                    }
                    return Some(ordered);
                }
                if report.outcome != TerminalOutcome::Partial {
                    return None;
                }
                let mut completed = HashSet::new();
                let mut terminal_items = HashSet::new();
                if report.receipts.iter().any(|receipt| {
                    receipt.item.0 as usize >= sources.len()
                        || !terminal_items.insert(receipt.item)
                }) {
                    return None;
                }
                for receipt in &report.receipts {
                    if receipt.outcome == super::model::ItemOutcome::Completed {
                        completed.insert(receipt.item);
                    }
                }
                if completed.len() <= 1 || receipts.len() != completed.len() {
                    return None;
                }

                let mut ordered = Vec::with_capacity(sources.len());
                for (index, source) in sources.iter().enumerate() {
                    let item = super::model::ItemId(index as u32);
                    if !completed.contains(&item) {
                        continue;
                    }
                    let facts = receipts.iter().filter(|(candidate, _)| *candidate == item).collect::<Vec<_>>();
                    if facts.len() != 1 || facts[0].1.original_path() != *source {
                        return None;
                    }
                    ordered.push(facts[0].1.clone());
                }
                if ordered.len() != receipts.len() {
                    return None;
                }
                Some(ordered)
            });
            if let Some(group) = group {
                self.undo_ledger.push(UndoEntry::TrashGroup(group));
            } else {
                self.undo_ledger.extend(receipts.into_iter().map(|(_, receipt)| UndoEntry::Trash(receipt)));
            }
            self.trim_undo_ledger();
        }
        let copy_job = self.service.job(token.job_id()).filter(|job| {
            job.request.kind == super::model::OperationKind::Copy
                && job.request.virtual_source.is_none()
                && !job.request.sources.is_empty()
        });
        if let Some(job) = copy_job {
            // 初回 attempt の分類を request 単位で保持し、retry の subset や再検査結果で
            // 元の mixed / indeterminate 判定を弱めない。
            if job.attempts.first().is_some_and(|attempt| {
                attempt.token == *token && attempt.retry_items.is_empty()
            }) {
                let original_kinds = (0..job.request.sources.len())
                    .map(|index| self.ordinary_copy_source_kinds
                        .get(&(*token, super::model::ItemId(index as u32)))
                        .copied().unwrap_or(super::executor::OrdinaryCopySourceKind::Indeterminate))
                    .collect::<Vec<_>>();
                self.ordinary_copy_source_kinds_by_job_items
                    .entry(token.job_id()).or_insert(original_kinds);
            }
            self.ordinary_copy_source_kinds_by_job
                .entry(token.job_id())
                .or_insert_with(|| {
                    let mut has_file = false;
                    let mut has_directory = false;
                    let mut indeterminate = false;
                    for kind in self.ordinary_copy_source_kinds_by_job_items
                        .get(&token.job_id()).into_iter().flatten() {
                        match kind {
                            super::executor::OrdinaryCopySourceKind::File => has_file = true,
                            super::executor::OrdinaryCopySourceKind::Directory => has_directory = true,
                            super::executor::OrdinaryCopySourceKind::Indeterminate => indeterminate = true,
                        }
                    }
                    match (has_file, has_directory, indeterminate) {
                        (true, true, false) => OrdinaryCopyRequestClassification::Mixed,
                        (true, true, true) => OrdinaryCopyRequestClassification::MixedIncomplete,
                        (true, false, false) => OrdinaryCopyRequestClassification::AllFiles,
                        (false, true, false) => OrdinaryCopyRequestClassification::AllDirectories,
                        _ => OrdinaryCopyRequestClassification::Indeterminate,
                    }
                });
        }
        let copy_classification = self
            .ordinary_copy_source_kinds_by_job
            .get(&token.job_id())
            .map(|classification| {
                let mixed = matches!(classification,
                    OrdinaryCopyRequestClassification::Mixed
                        | OrdinaryCopyRequestClassification::MixedIncomplete);
                let complete = matches!(classification,
                    OrdinaryCopyRequestClassification::Mixed
                        | OrdinaryCopyRequestClassification::AllFiles
                        | OrdinaryCopyRequestClassification::AllDirectories);
                (mixed, complete)
            });
        let mixed_copy_detected = copy_classification.is_some_and(|(mixed, _)| mixed);
        let copy_classification_complete = copy_classification.is_some_and(|(_, complete)| complete);
        let file_copy_group_candidate = self.ordinary_copy_source_kinds_by_job.get(&token.job_id())
            == Some(&OrdinaryCopyRequestClassification::AllFiles)
            && self.service.job(token.job_id()).is_some_and(|job| job.request.sources.len() > 1)
            && report.outcome == TerminalOutcome::Succeeded;
        let guarded_file_copy_retry = self.service.job(token.job_id()).is_some_and(|job| {
            job.request.kind == super::model::OperationKind::Copy
                && job.request.copy_destination_identity_guard.is_some()
                && matches!(self.ordinary_copy_source_kinds_by_job.get(&token.job_id()),
                    Some(OrdinaryCopyRequestClassification::AllFiles | OrdinaryCopyRequestClassification::AllDirectories | OrdinaryCopyRequestClassification::Mixed))
                && job.attempts.last().is_some_and(|attempt| {
                    attempt.token == *token
                        && !attempt.retry_items.is_empty()
                        && attempt.retry_items.iter().copied().collect::<HashSet<_>>().len()
                            == attempt.retry_items.len()
                        && attempt.retry_items.iter().all(|item| {
                            usize::try_from(item.0).ok()
                                .is_some_and(|index| index < job.request.sources.len())
                        })
                        && (attempt.copy_retry_guards.len() + attempt.copy_directory_retry_guards.len()) == attempt.retry_items.len()
                        && !attempt.retry_items.is_empty()
                })
        });
        let mixed_copy_undo = self.service.job(token.job_id()).and_then(|job| {
            let homogeneous_files = self.ordinary_copy_source_kinds_by_job.get(&token.job_id())
                == Some(&OrdinaryCopyRequestClassification::AllFiles)
                && job.request.sources.len() > 1;
            let has_replace_receipt = report.receipts.iter().any(|receipt| receipt.replace_receipt.is_some());
            let guarded_file_group = homogeneous_files
                && job.request.copy_destination_identity_guard.is_some()
                && job.attempts.len() == 1;
            let guarded_mixed_group = self.ordinary_copy_source_kinds_by_job.get(&token.job_id())
                == Some(&OrdinaryCopyRequestClassification::Mixed)
                && job.request.copy_destination_identity_guard.is_some()
                && job.attempts.len() == 1;
            let unguarded_mixed_partial = job.request.kind == super::model::OperationKind::Copy
                && report.outcome == TerminalOutcome::Partial
                && job.request.sources.len() > 1
                && job.request.virtual_source.is_none()
                && job.request.copy_destination_identity_guard.is_none()
                && self.ordinary_copy_source_kinds_by_job.get(&token.job_id())
                    == Some(&OrdinaryCopyRequestClassification::Mixed)
                && job.attempts.len() == 1
                && job.attempts[0].token == *token
                && job.attempts[0].retry_items.is_empty();
            let partial_file_replace_group = job.request.kind == super::model::OperationKind::Copy
                && report.outcome == TerminalOutcome::Partial
                && homogeneous_files
                && has_replace_receipt
                && job.request.virtual_source.is_none()
                && job.attempts.len() == 1
                && job.attempts[0].token == *token
                && job.attempts[0].retry_items.is_empty();
            let mixed_partial_group = guarded_mixed_group || unguarded_mixed_partial;
            let partial_copy_group = mixed_partial_group || partial_file_replace_group;
            let validate_mixed_outputs = guarded_mixed_group || unguarded_mixed_partial || partial_file_replace_group;
            let guarded_group = guarded_file_group || guarded_mixed_group;
            let guarded_destination_identity = job.request.copy_destination_identity_guard.as_ref()
                .and_then(|guard| guard.windows_parts())
                .map(|(volume, index)| format!("{volume}:{index:016x}"));
            if guarded_file_copy_retry
                || !(mixed_copy_detected || homogeneous_files) || !copy_classification_complete
                || (report.outcome != TerminalOutcome::Succeeded
                    && !(report.outcome == TerminalOutcome::Partial && partial_copy_group))
                || (has_replace_receipt
                    && (job.attempts.len() != 1 || !job.attempts[0].retry_items.is_empty()))
                || (has_replace_receipt && !homogeneous_files)
                || (guarded_mixed_group && !job.attempts[0].retry_items.is_empty())
                || (guarded_mixed_group && job.attempts[0].token != *token)
                || (job.request.copy_destination_identity_guard.is_some() && !guarded_group)
                || report.receipts.len() != job.request.sources.len()
                || job.request.sources.is_empty()
                || (report.outcome == TerminalOutcome::Partial
                    && (!partial_copy_group
                        || !report.receipts.iter().any(|receipt| receipt.outcome == super::model::ItemOutcome::Completed)
                        || !report.receipts.iter().any(|receipt| receipt.outcome != super::model::ItemOutcome::Completed)))
                || (guarded_group && guarded_destination_identity.is_none())
                || self.directory_copy_undo_candidates.keys().any(|(candidate_token, item)| {
                    *candidate_token == *token && item.0 as usize >= job.request.sources.len()
                })
                || self.invalid_directory_copy_undo_candidates.iter().any(|(candidate_token, item)| {
                    *candidate_token == *token && item.0 as usize >= job.request.sources.len()
                }) { return None; }
            let mut items = Vec::with_capacity(job.request.sources.len());
            let mut saw_file = false;
            let mut saw_directory = false;
            let mut saw_completed_replace = false;
            let mut seen = HashSet::with_capacity(report.receipts.len());
            let mut file_identities = HashSet::with_capacity(report.receipts.len());
            let mut destination_leaves: Vec<&std::ffi::OsStr> = Vec::with_capacity(report.receipts.len());
            for (index, item_receipt) in report.receipts.iter().enumerate() {
                let item = super::model::ItemId(index as u32);
                if item_receipt.item != item || !seen.insert(item) {
                    return None;
                }
                let key = (*token, item);
                let directory = self.directory_copy_undo_candidates.get(&key);
                if self.invalid_directory_copy_undo_candidates.contains(&key) { return None; }
                if item_receipt.outcome != super::model::ItemOutcome::Completed {
                    if report.outcome != TerminalOutcome::Partial
                        || item_receipt.copy_undo.is_some()
                        || directory.is_some()
                    { return None; }
                    continue;
                }
                match (&item_receipt.copy_undo, directory) {
                    (Some(file), None) => {
                        if validate_mixed_outputs
                            && self.ordinary_copy_source_kinds.get(&key)
                                != Some(&super::executor::OrdinaryCopySourceKind::File)
                        { return None; }
                        let base = job.request.destination.as_ref()?.join(job.request.sources[index].file_name()?);
                        let requested_leaf = job.request.sources[index].file_name()?;
                        let replace_is_valid = item_receipt.replace_receipt.as_ref().map_or(true, |replace| {
                            replace.source == job.request.sources[index]
                                && replace.destination == base
                                && replace.destination.parent() == job.request.destination.as_deref()
                                && replace.displaced_backup.is_some()
                                && replace.destination_published
                                && replace.displaced_backup_removed
                        });
                        saw_completed_replace |= item_receipt.replace_receipt.is_some();
                        if file.destination.parent() != job.request.destination.as_deref()
                            || (if guarded_group || has_replace_receipt || unguarded_mixed_partial {
                                file.destination != base
                                    && (!(guarded_mixed_group || unguarded_mixed_partial)
                                        || !self.keep_both_directory_items.contains(&key)
                                        || !super::model::is_keep_both_candidate_path(&base, &file.destination))
                            } else {
                                file.destination != base
                                    && !super::model::is_keep_both_candidate_path(&base, &file.destination)
                            })
                            || (guarded_group
                                && guarded_destination_identity.as_deref() != Some(file.parent_identity.as_str()))
                            || !replace_is_valid
                            || (has_replace_receipt && file.destination.file_name().map_or(true, |leaf| {
                                !windows_ordinal_leaf_eq(leaf, requested_leaf)
                            }))
                            || (has_replace_receipt && !file_identities.insert(file.file_identity.as_str()))
                            || (has_replace_receipt && file.destination.file_name().map_or(true, |leaf| {
                                destination_leaves.iter().any(|previous| windows_ordinal_leaf_eq(previous, leaf))
                            }))
                        { return None; }
                        if has_replace_receipt || validate_mixed_outputs {
                            destination_leaves.push(file.destination.file_name()?);
                        }
                        saw_file = true;
                        items.push(super::model::CopyUndoItem::File(file.clone()));
                    }
                    (None, Some(directory)) => {
                        if validate_mixed_outputs
                            && self.ordinary_copy_source_kinds.get(&key)
                                != Some(&super::executor::OrdinaryCopySourceKind::Directory)
                        { return None; }
                        let base = job.request.destination.as_ref()?.join(job.request.sources[index].file_name()?);
                        let directory_parent_matches = if guarded_mixed_group {
                            job.request.copy_destination_identity_guard.as_ref()
                                .is_some_and(|guard| directory.destination_parent_route.last().is_some_and(|parent| {
                                    Some(parent.path.as_path()) == job.request.destination.as_deref()
                                        && Some(parent.path.as_path()) == directory.destination.parent()
                                        && &parent.identity == guard
                                }))
                        } else if unguarded_mixed_partial {
                            directory.destination_parent_route.last().is_some_and(|parent| {
                                Some(parent.path.as_path()) == job.request.destination.as_deref()
                                    && Some(parent.path.as_path()) == directory.destination.parent()
                                    && parent.identity.windows_parts().is_some()
                            })
                        } else { true };
                        if directory.destination.parent() != job.request.destination.as_deref()
                            || !directory_parent_matches
                            || (directory.destination != base && (!self.keep_both_directory_items.contains(&key)
                                || !super::model::is_keep_both_directory_candidate_path(&base, &directory.destination)))
                        { return None; }
                        if validate_mixed_outputs {
                            destination_leaves.push(directory.destination.file_name()?);
                        }
                        saw_directory = true;
                        items.push(super::model::CopyUndoItem::Directory(directory.clone()));
                    }
                    _ => return None,
                }
            }
            if partial_file_replace_group && !saw_completed_replace { return None; }
            if validate_mixed_outputs {
                let mut object_identities = HashSet::with_capacity(items.len());
                let mut common_parent_route: Option<&[super::model::SourceCleanupParentIdentity]> = None;
                for item in &items {
                    let (leaf, identity) = match item {
                        super::model::CopyUndoItem::File(file) => {
                            (file.destination.file_name()?, file.file_identity.as_str())
                        }
                        super::model::CopyUndoItem::Directory(directory) => {
                            match common_parent_route {
                                Some(route) if route != directory.destination_parent_route.as_slice() => return None,
                                None => common_parent_route = Some(&directory.destination_parent_route),
                                _ => {}
                            }
                            (directory.destination.file_name()?, directory.root_identity.as_str())
                        }
                    };
                    if !object_identities.insert(identity)
                        || destination_leaves.iter().filter(|previous| windows_ordinal_leaf_eq(previous, leaf)).count() != 1
                    { return None; }
                }
            }
            ((report.outcome == TerminalOutcome::Partial && partial_copy_group)
                || (saw_file && saw_directory)
                || (homogeneous_files && saw_file && !saw_directory
                    && (job.request.copy_destination_identity_guard.is_none() || guarded_file_group)))
                .then_some(items)
        });
        // exact group admission に失敗した file batch は、従来どおり個別 receipt へ fallback する。
        let multi_source_replace_evidence = report.receipts.iter().any(|receipt| receipt.replace_receipt.is_some())
            && self.service.job(token.job_id()).is_some_and(|job| job.request.sources.len() > 1);
        let guarded_directory_copy = self.service.job(token.job_id()).is_some_and(|job| {
            job.request.copy_destination_identity_guard.is_some()
                && self.ordinary_copy_source_kinds_by_job.get(&token.job_id())
                    == Some(&OrdinaryCopyRequestClassification::AllDirectories)
        });
        let suppress_homogeneous_copy_undo = mixed_copy_detected || !copy_classification_complete
            || (file_copy_group_candidate && mixed_copy_undo.is_some())
            || multi_source_replace_evidence
            || guarded_directory_copy
            || guarded_file_copy_retry;
        // Guarded directory batches have their own exact proof admission below; keep the
        // homogeneous per-item fallback suppressed while allowing that group validator to run.
        let suppress_directory_copy_undo = mixed_copy_detected || !copy_classification_complete
            || multi_source_replace_evidence
            || guarded_file_copy_retry
            || (mixed_copy_undo.is_some() && !guarded_directory_copy);
        self.directory_copy_undo_candidates.retain(|(candidate_token, _), _| *candidate_token != *token || mixed_copy_undo.is_none());
        self.invalid_directory_copy_undo_candidates.retain(|(candidate_token, _)| *candidate_token != *token || mixed_copy_undo.is_none());
        if let Some(items) = mixed_copy_undo {
            self.undo_ledger.push(UndoEntry::MixedCopyGroup(items));
            self.trim_undo_ledger();
        }
        let copy_undo_receipts = if suppress_homogeneous_copy_undo { Vec::new() } else { self
            .service
            .job(token.job_id())
            .filter(|job| {
                job.request.kind == super::model::OperationKind::Copy
                    && job.request.virtual_source.is_none()
                    && matches!(
                        report.outcome,
                        TerminalOutcome::Succeeded | TerminalOutcome::Partial
                    )
            })
            .map(|_| {
                report
                    .receipts
                    .iter()
                    .filter(|receipt| receipt.outcome == super::model::ItemOutcome::Completed)
                    // Replace は単独成功の専用判定でだけ受け付け、履歴 receipt 自体には Undo 権限を持たせない。
                    .filter(|receipt| receipt.replace_receipt.is_none())
                    .filter_map(|receipt| receipt.copy_undo.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default() };
        if !copy_undo_receipts.is_empty() {
            self.undo_ledger
                .extend(copy_undo_receipts.into_iter().map(UndoEntry::Copy));
            self.trim_undo_ledger();
        }
        let replace_copy_undo = self.service.job(token.job_id()).and_then(|job| {
            if job.request.kind != super::model::OperationKind::Copy
                || job.request.virtual_source.is_some()
                || job.request.sources.len() != 1
                || report.outcome != super::model::TerminalOutcome::Succeeded
                || report.receipts.len() != 1
                || job.attempts.len() != 1
                || !job.attempts[0].retry_items.is_empty()
                || self.ordinary_copy_source_kinds.get(&(*token, super::model::ItemId(0)))
                    != Some(&super::executor::OrdinaryCopySourceKind::File)
            {
                return None;
            }
            let receipt = report.receipts.first()?;
            if receipt.item != super::model::ItemId(0)
                || receipt.outcome != super::model::ItemOutcome::Completed
            {
                return None;
            }
            let replace = receipt.replace_receipt.as_ref()?;
            let source = &job.request.sources[0];
            let destination_parent = job.request.destination.as_ref()?;
            let expected_destination = destination_parent.join(source.file_name()?);
            let proof = receipt.copy_undo.as_ref()?;
            (replace.source == *source
                && replace.destination == expected_destination
                && replace.destination.parent() == Some(destination_parent.as_path())
                && replace.displaced_backup.is_some()
                && replace.destination_published
                && replace.displaced_backup_removed
                && proof.destination == expected_destination)
                .then(|| proof.clone())
        });
        if let Some(receipt) = replace_copy_undo {
            self.undo_ledger.push(UndoEntry::Copy(receipt));
            self.trim_undo_ledger();
        }
        let directory_copy_undo = self.service.job(token.job_id()).and_then(|job| {
            let guarded_destination_identity_guard = job.request.copy_destination_identity_guard.as_ref();
            if job.request.kind != super::model::OperationKind::Copy
                || job.request.sources.is_empty()
                || job.request.virtual_source.is_some()
                || suppress_directory_copy_undo
                || report.receipts.iter().any(|receipt| receipt.replace_receipt.is_some())
                || guarded_destination_identity_guard.is_some_and(|_| {
                    job.attempts.len() != 1
                        || job.attempts[0].token != *token
                        || (report.outcome == super::model::TerminalOutcome::Partial
                            && !job.attempts[0].retry_items.is_empty())
                        || (report.outcome != super::model::TerminalOutcome::Succeeded
                            && report.outcome != super::model::TerminalOutcome::Partial)
                        || report.receipts.len() != job.request.sources.len()
                        || report.receipts.iter().enumerate().any(|(index, receipt)| {
                            receipt.item != super::model::ItemId(index as u32)
                                || (report.outcome == super::model::TerminalOutcome::Succeeded
                                    && receipt.outcome != super::model::ItemOutcome::Completed)
                                || receipt.replace_receipt.is_some()
                        })
                        || self.ordinary_copy_source_kinds_by_job.get(&token.job_id())
                            != Some(&OrdinaryCopyRequestClassification::AllDirectories)
                })
                || (report.outcome == super::model::TerminalOutcome::Partial && job.request.sources.len() < 2)
                || (report.outcome == super::model::TerminalOutcome::Partial
                    && report.receipts.iter().all(|receipt| receipt.outcome == super::model::ItemOutcome::Completed))
                || !matches!(report.outcome, super::model::TerminalOutcome::Succeeded | super::model::TerminalOutcome::Partial)
            {
                return None;
            }
            if report.receipts.len() != job.request.sources.len()
                || (report.outcome == super::model::TerminalOutcome::Succeeded
                    && report.receipts.iter().any(|receipt| receipt.outcome != super::model::ItemOutcome::Completed))
                || (report.outcome == super::model::TerminalOutcome::Partial
                    && !report.receipts.iter().any(|receipt| receipt.outcome == super::model::ItemOutcome::Completed))
                || (guarded_destination_identity_guard.is_some()
                    && report.outcome == super::model::TerminalOutcome::Partial
                    && !report.receipts.iter().any(|receipt| receipt.outcome != super::model::ItemOutcome::Completed))
            {
                return None;
            }
            let mut completed_ids = HashSet::new();
            let mut all_ids = HashSet::with_capacity(report.receipts.len());
            let mut proofs = Vec::with_capacity(job.request.sources.len());
            for (index, receipt) in report.receipts.iter().enumerate() {
                if receipt.item != super::model::ItemId(index as u32)
                    || !all_ids.insert(receipt.item)
                { return None; }
                if receipt.outcome != super::model::ItemOutcome::Completed { continue; }
                completed_ids.insert(receipt.item);
                let key = (*token, receipt.item);
                if self.invalid_directory_copy_undo_candidates.contains(&key) { return None; }
                let proof = self.directory_copy_undo_candidates.get(&key)?;
                let base = job.request.destination.as_ref()?.join(
                    job.request.sources[index].file_name().unwrap_or_default(),
                );
                let original_leaf = job.request.sources[index].file_name() == proof.destination.file_name();
                let guarded_parent_matches = guarded_destination_identity_guard.is_none_or(|guard| {
                    proof.destination_parent_route.last().is_some_and(|parent| {
                        Some(parent.path.as_path()) == job.request.destination.as_deref()
                            && Some(parent.path.as_path()) == proof.destination.parent()
                            && &parent.identity == guard
                    })
                });
                if job.request.destination.as_deref() != proof.destination.parent()
                    || !guarded_parent_matches
                    || (!original_leaf
                        && (!self.keep_both_directory_items.contains(&key)
                            || !super::model::is_keep_both_directory_candidate_path(&base, &proof.destination)))
                { return None; }
                proofs.push((key, proof.clone()));
            }
            if self.directory_copy_undo_candidates.keys().any(|(candidate_token, item)| {
                *candidate_token == *token && !completed_ids.contains(item)
            }) { return None; }
            if self.invalid_directory_copy_undo_candidates.iter().any(|(candidate_token, item)| {
                *candidate_token == *token && !completed_ids.contains(item)
            }) { return None; }
            if proofs.len() != completed_ids.len() { return None; }
            if guarded_destination_identity_guard.is_some() {
                let mut destination_leaves: Vec<&std::ffi::OsStr> = Vec::with_capacity(proofs.len());
                let mut root_identities = HashSet::with_capacity(proofs.len());
                let mut common_parent_route: Option<&[super::model::SourceCleanupParentIdentity]> = None;
                for (_, proof) in &proofs {
                    let leaf = proof.destination.file_name()?;
                    if destination_leaves.iter().any(|previous| windows_ordinal_leaf_eq(previous, leaf))
                        || !root_identities.insert(proof.root_identity.as_str())
                    { return None; }
                    destination_leaves.push(leaf);
                    match common_parent_route {
                        Some(route) if route != proof.destination_parent_route.as_slice() => return None,
                        None => common_parent_route = Some(&proof.destination_parent_route),
                        _ => {}
                    }
                }
            }
            for (key, _) in &proofs { self.directory_copy_undo_candidates.remove(key); }
            let proofs = proofs.into_iter().map(|(_, proof)| proof).collect::<Vec<_>>();
            if job.request.sources.len() == 1 { Some((false, proofs)) }
            else { Some((true, proofs)) }
        });
        self.directory_copy_undo_candidates.retain(|(candidate_token, _), _| candidate_token != token);
        self.invalid_directory_copy_undo_candidates.retain(|(candidate_token, _)| candidate_token != token);
        self.ordinary_copy_source_kinds.retain(|(candidate_token, _), _| candidate_token != token);
        if let Some((group, mut receipts)) = directory_copy_undo {
            if group { self.undo_ledger.push(UndoEntry::DirectoryCopyGroup(receipts)); }
            else if let Some(receipt) = receipts.pop() { self.undo_ledger.push(UndoEntry::DirectoryCopy(receipt)); }
            self.trim_undo_ledger();
        }
        let created_undo = self.service.job(token.job_id()).and_then(|job| {
            if report.outcome != super::model::TerminalOutcome::Succeeded {
                return None;
            }
            if job.request.kind == super::model::OperationKind::CreateShortcut {
                if report.receipts.len() != 1 || self.invalid_created_object_undo_candidates.contains(&(*token, super::model::ItemId(0))) {
                    return None;
                }
                let item_receipt = &report.receipts[0];
                let item = super::model::ItemId(0);
                let expected_path = job.request.shortcut_output_path()?;
                let expected_detail = expected_path.display().to_string();
                if item_receipt.item != item
                    || item_receipt.outcome != super::model::ItemOutcome::Completed
                    || item_receipt.detail.as_deref() != Some(expected_detail.as_str())
                    || item_receipt.created_folder_path.is_some()
                    || item_receipt.created_archive_path.is_some()
                    || item_receipt.move_receipt.is_some()
                    || item_receipt.replace_receipt.is_some()
                    || item_receipt.copy_undo.is_some()
                {
                    return None;
                }
                let proof = self.created_object_undo_candidates.remove(&(*token, item))?;
                let guard = job.request.shortcut_create_guard.as_ref()?;
                let parent_matches = guard.parent_identity.windows_parts().is_some_and(|(volume, index)| {
                    proof.parent_identity
                        == format!("{}:{:08x}{:08x}", volume, (index >> 32) as u32, index as u32)
                });
                let proof_kind_matches = match &proof.kind {
                    super::model::CreatedObjectUndoKind::ShortcutFile { content_sha256, signature, size } => {
                        *size > 0
                            && *size <= 1024 * 1024
                            && signature.number_of_links == 1
                            && signature.change_time != 0
                            && content_sha256.len() == 64
                            && content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }
                    _ => false,
                };
                return (job.request.shortcut_create_shape_is_valid()
                    && proof.destination == expected_path
                    && parent_matches
                    && proof_kind_matches)
                    .then_some(proof);
            }
            if !matches!(job.request.kind, super::model::OperationKind::CreateFile | super::model::OperationKind::CreateFolder) {
                return None;
            }
            let mut completed = report.receipts.iter().filter(|r| r.outcome == super::model::ItemOutcome::Completed);
            let item_receipt = completed.next()?;
            if completed.next().is_some() { return None; }
            let key = (*token, item_receipt.item);
            let proof = self.created_object_undo_candidates.remove(&key)?;
            let expected_path = match job.request.kind {
                super::model::OperationKind::CreateFolder => item_receipt.created_folder_path.clone(),
                super::model::OperationKind::CreateFile => item_receipt.detail.as_deref()
                    .and_then(|name| job.request.destination.as_ref().map(|parent| parent.join(name))),
                _ => None,
            };
            let kind_matches = matches!((&job.request.kind, &proof.kind),
                (super::model::OperationKind::CreateFile, super::model::CreatedObjectUndoKind::EmptyFile { .. })
                    | (super::model::OperationKind::CreateFolder, super::model::CreatedObjectUndoKind::EmptyFolder));
            (kind_matches && expected_path.as_ref() == Some(&proof.destination)).then_some(proof)
        });
        self.created_object_undo_candidates.retain(|(candidate_token, _), _| candidate_token != token);
        self.invalid_created_object_undo_candidates.retain(|(candidate_token, _)| candidate_token != token);
        if let Some(receipt) = created_undo {
            self.undo_ledger.push(UndoEntry::CreatedObject(receipt));
            self.trim_undo_ledger();
        }
        let move_undo = self.service.job(token.job_id()).and_then(|job| {
            if job.request.kind == super::model::OperationKind::Move
                && job.request.move_replace_source_evidence.is_some()
                && job.request.sources.len() > 1
                && report.receipts.iter().any(|receipt| receipt.replace_receipt.is_some())
            {
                let count = job.request.sources.len();
                let evidence = job.request.move_replace_source_evidence.as_ref()?;
                if report.outcome != super::model::TerminalOutcome::Succeeded
                    || count < 2
                    || evidence.len() != count
                    || job.request.virtual_source.is_some()
                    || job.request.batch_rename_plan.is_some()
                    || job.attempts.len() != 1
                    || !job.attempts[0].retry_items.is_empty()
                    || self.service.cross_volume_retry_authority(*token).is_some()
                    || report.receipts.len() != count
                    || self.invalid_move_undo_candidates.iter().any(|(candidate_token, _)| candidate_token == token)
                    || self.move_undo_candidates.keys().filter(|(candidate_token, _)| candidate_token == token).count() != count
                { return None; }

                let mut by_item = HashMap::with_capacity(count);
                for receipt in &report.receipts {
                    if receipt.item.0 as usize >= count || by_item.insert(receipt.item, receipt).is_some() { return None; }
                }
                let mut selected = Vec::with_capacity(count);
                let mut destination_leaves = Vec::with_capacity(count);
                let destination_parent = job.request.destination.as_deref()?;
                let destination_identity = job.request.move_destination_identity_guard.as_ref()?;
                for index in 0..count {
                    let item = super::model::ItemId(index as u32);
                    let receipt = *by_item.get(&item)?;
                    if receipt.outcome != super::model::ItemOutcome::Completed
                        || receipt.created_folder_path.is_some() || receipt.created_archive_path.is_some()
                        || receipt.copy_undo.is_some() { return None; }
                    let source = &job.request.sources[index];
                    let candidate = self.move_undo_candidates.get(&(*token, item))?.clone();
                    let expected = source.file_name().map(|leaf| destination_parent.join(leaf))?;
                    if candidate.source != *source || candidate.destination != expected
                        || candidate.object_kind != super::model::MoveUndoObjectKind::RegularFile
                        || candidate.cross_volume_file.is_some()
                        || candidate.source_parent_route.last().is_none_or(|route| Some(route.path.as_path()) != source.parent())
                        || candidate.destination_parent_route.last().is_none_or(|route| route.path != destination_parent || &route.identity != destination_identity)
                        || evidence.get(index).is_none_or(|proof| proof.source_identity != candidate.object_identity)
                    { return None; }
                    let leaf = candidate.destination.file_name()?;
                    if destination_leaves.iter().any(|previous: &std::ffi::OsString| windows_ordinal_leaf_eq(previous.as_os_str(), leaf)) { return None; }
                    destination_leaves.push(leaf.to_os_string());
                    match (&receipt.move_receipt, &receipt.replace_receipt) {
                        (None, Some(replace)) if replace.source == *source
                            && replace.destination == candidate.destination
                            && replace.destination.parent() == Some(destination_parent)
                            && replace.destination.file_name() == source.file_name()
                            && replace.destination_published && replace.displaced_backup.is_some()
                            && replace.displaced_backup_removed => {},
                        (Some(moved), None) if moved.source == *source
                            && moved.destination == candidate.destination
                            && moved.destination_published && moved.source_removed => {},
                        _ => return None,
                    }
                    selected.push(candidate);
                }
                return Some(selected);
            } else if job.request.kind == super::model::OperationKind::Move
                && job.request.move_replace_source_evidence.as_ref().is_some_and(|evidence| evidence.len() == job.request.sources.len())
                && job.request.sources.len() == 1
                && report.receipts.first().is_some_and(|receipt| receipt.replace_receipt.is_some())
            {
                // Existing singleton Replace admission remains process-local and exact.
                if report.outcome != super::model::TerminalOutcome::Succeeded
                    || job.request.virtual_source.is_some() || job.request.batch_rename_plan.is_some()
                    || job.attempts.len() != 1 || !job.attempts[0].retry_items.is_empty()
                    || self.service.cross_volume_retry_authority(*token).is_some()
                    || report.receipts.len() != 1 || report.receipts[0].item != super::model::ItemId(0)
                    || report.receipts[0].outcome != super::model::ItemOutcome::Completed
                    || self.invalid_move_undo_candidates.contains(&(*token, super::model::ItemId(0)))
                { return None; }
                let receipt = &report.receipts[0];
                let replace = receipt.replace_receipt.as_ref()?;
                if receipt.move_receipt.is_some() || receipt.created_folder_path.is_some() || receipt.created_archive_path.is_some()
                    || receipt.copy_undo.is_some() || replace.source != job.request.sources[0]
                    || replace.destination.parent() != job.request.destination.as_deref()
                    || replace.destination.file_name() != job.request.sources[0].file_name()
                    || !replace.destination_published || !replace.displaced_backup_removed || replace.displaced_backup.is_none()
                { return None; }
                let candidate = self.move_undo_candidates.get(&(*token, super::model::ItemId(0)))?.clone();
                return (candidate.source == job.request.sources[0] && candidate.destination == replace.destination
                    && candidate.object_kind == super::model::MoveUndoObjectKind::RegularFile && candidate.cross_volume_file.is_none()
                    && candidate.source_parent_route.last().is_some_and(|parent| Some(parent.path.as_path()) == candidate.source.parent())
                    && candidate.destination_parent_route.last().is_some_and(|parent| Some(parent.path.as_path()) == replace.destination.parent() && job.request.move_destination_identity_guard.as_ref() == Some(&parent.identity))
                    && job.request.move_replace_source_evidence.as_ref().and_then(|items| items.first()).is_some_and(|proof| proof.source_identity == candidate.object_identity)).then_some(vec![candidate]);
            } else if job.request.kind == super::model::OperationKind::Rename {
                if report.outcome != super::model::TerminalOutcome::Succeeded
                    || job.request.sources.len() != 1
                    || job.request.virtual_source.is_some()
                    || job.request.rename_reparse_guard.is_some()
                    || job.request.move_replace_source_evidence.is_some()
                    || job.request.move_destination_identity_guard.is_some()
                    || job.request.batch_rename_plan.is_some()
                    || job.attempts.len() != 1
                    || self.service.cross_volume_retry_authority(*token).is_some()
                    || report.receipts.len() != 1
                    || report.receipts[0].item != super::model::ItemId(0)
                    || report.receipts[0].outcome != super::model::ItemOutcome::Completed
                    || report.receipts[0].replace_receipt.is_some()
                    || self.invalid_move_undo_candidates.contains(&(*token, super::model::ItemId(0)))
                    || self.invalid_move_undo_candidates.iter().any(|(candidate_token, _)| candidate_token == token)
                    || self.move_undo_candidates.keys().filter(|(candidate_token, _)| candidate_token == token).count() != 1
                { return None; }
                let candidate = self.move_undo_candidates.get(&(*token, super::model::ItemId(0)))?.clone();
                let source = job.request.sources.first()?;
                let destination = job.request.destination.as_ref()?;
                let guard = job.request.move_identity_guard.as_ref()?;
                return (candidate.source == *source
                    && candidate.destination == *destination
                    && candidate.destination.parent() == source.parent()
                    && match candidate.object_kind {
                        super::model::MoveUndoObjectKind::CaseOnlyRename => candidate.destination.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| super::rename::is_case_only_rename(source, name)),
                        super::model::MoveUndoObjectKind::RegularFile
                        | super::model::MoveUndoObjectKind::Directory => candidate.destination.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| !super::rename::is_case_only_rename(source, name)),
                        super::model::MoveUndoObjectKind::ReparsePoint { .. } => false,
                    }
                    && guard.source_identities.len() == 1
                    && guard.source_identities[0] == candidate.object_identity
                    && candidate.source_parent_route.last().is_some_and(|route| {
                        Some(route.path.as_path()) == source.parent()
                            && &route.identity == &guard.destination_identity
                    })
                    && candidate.destination_parent_route.last().is_some_and(|route| {
                        Some(route.path.as_path()) == destination.parent()
                            && &route.identity == &guard.destination_identity
                    })
                    && candidate.cross_volume_file.is_none())
                    .then(|| vec![candidate]);
            }
            if job.request.kind != super::model::OperationKind::Move
                || !matches!(report.outcome, super::model::TerminalOutcome::Succeeded | super::model::TerminalOutcome::Partial) { return None; }
            // Cut/Paste は source identity を捕捉しないため、要求位置との対応を
            // 検証し終えるまで ItemId と候補を組のまま保持する。
            if job.request.move_destination_identity_guard.is_some()
                && job.request.move_identity_guard.is_none()
            {
                let source_count = job.request.sources.len();
                if source_count == 0
                    || report.receipts.len() != source_count
                    || self.service.cross_volume_retry_authority(*token).is_some()
                    || self.invalid_move_undo_candidates.iter().any(|(candidate_token, _)| candidate_token == token)
                {
                    return None;
                }

                let mut receipt_by_item = HashMap::with_capacity(source_count);
                for item in &report.receipts {
                    if item.item.0 as usize >= source_count || receipt_by_item.insert(item.item, item).is_some() {
                        return None;
                    }
                }
                if receipt_by_item.len() != source_count {
                    return None;
                }

                // 非Completed itemの候補をfilterで隠さず、group全体を拒否する。
                for (candidate_token, item_id) in self.move_undo_candidates.keys() {
                    if candidate_token == token
                        && (item_id.0 as usize >= source_count
                            || receipt_by_item.get(item_id).is_none_or(|item| item.outcome != super::model::ItemOutcome::Completed))
                    {
                        return None;
                    }
                }

                let completed_count = receipt_by_item.values()
                    .filter(|item| item.outcome == super::model::ItemOutcome::Completed)
                    .count();
                if completed_count == 0
                    || (report.outcome == super::model::TerminalOutcome::Succeeded && completed_count != source_count)
                {
                    return None;
                }

                let mut selected = Vec::with_capacity(completed_count);
                for index in 0..source_count {
                    let item_id = super::model::ItemId(index as u32);
                    let item = receipt_by_item.get(&item_id).copied()?;
                    if item.replace_receipt.is_some() { return None; }
                    if item.outcome != super::model::ItemOutcome::Completed { continue; }

                    let candidate = self.move_undo_candidates.get(&(*token, item_id))?.clone();
                    let proof = item.move_receipt.as_ref()?;
                    if !proof.destination_published || !proof.source_removed
                        || proof.source != job.request.sources[index]
                        || proof.source != candidate.source
                        || proof.destination != candidate.destination
                    {
                        return None;
                    }
                    selected.push((item_id, candidate));
                }

                let exact_clipboard_group = selected.iter().all(|(item_id, candidate)| {
                    let index = item_id.0 as usize;
                    let source = &job.request.sources[index];
                    let base_destination = job.request.destination.as_ref()
                        .and_then(|parent| source.file_name().map(|leaf| parent.join(leaf)));
                    let destination_name_valid = base_destination.as_ref().is_some_and(|base| {
                        candidate.destination == *base
                            || super::model::is_keep_both_candidate_path(base, &candidate.destination)
                    });
                    let parent_route_valid = candidate.destination.parent() == job.request.destination.as_deref()
                        && candidate.destination_parent_route.last().is_some_and(|route| {
                            Some(&route.path) == job.request.destination.as_ref()
                                && Some(&route.identity) == job.request.move_destination_identity_guard.as_ref()
                        });
                    if !destination_name_valid || !parent_route_valid { return false; }

                    match candidate.cross_volume_file.as_ref() {
                        Some(cross) => {
                            cross.directory_manifest.is_some()
                                == (candidate.object_kind == super::model::MoveUndoObjectKind::Directory)
                        }
                        None => match candidate.object_kind {
                            super::model::MoveUndoObjectKind::RegularFile
                            | super::model::MoveUndoObjectKind::Directory => true,
                            // Case-only rename receipts are admitted by the
                            // dedicated singleton Rename path below.
                            super::model::MoveUndoObjectKind::CaseOnlyRename => false,
                            super::model::MoveUndoObjectKind::ReparsePoint { tag, is_directory } => {
                                matches!(tag, 0xA000_000C | 0xA000_0003)
                                    && (tag != 0xA000_0003 || is_directory)
                            }
                        },
                    }
                });
                let has_reparse_member = selected.iter().any(|(_, candidate)| {
                    matches!(candidate.object_kind, super::model::MoveUndoObjectKind::ReparsePoint { .. })
                });
                // Reparse Undo is granted only for the complete successful group; never
                // let the ordinary Partial subset path retain a completed link prefix.
                let reparse_group_is_complete_success = !has_reparse_member
                    || (report.outcome == super::model::TerminalOutcome::Succeeded
                        && completed_count == source_count);
                return (exact_clipboard_group && reparse_group_is_complete_success)
                    .then(|| selected.into_iter().map(|(_, candidate)| candidate).collect());
            }

            let mut receipt_by_item = HashMap::with_capacity(report.receipts.len());
            let mut seen_items = HashSet::with_capacity(report.receipts.len());
            let mut duplicate_items = HashSet::new();
            for item in &report.receipts {
                if seen_items.insert(item.item) {
                    receipt_by_item.insert(item.item, item);
                } else {
                    duplicate_items.insert(item.item);
                }
            }
            let completed_items = receipt_by_item.values().filter(|item| item.outcome == super::model::ItemOutcome::Completed).count();
            let retry_authority = self.service.cross_volume_retry_authority(*token);
            let completed_outside_retry = retry_authority.as_ref().is_some_and(|(retry_items, _)| {
                receipt_by_item.iter().any(|(item_id, item)| item.outcome == super::model::ItemOutcome::Completed && !retry_items.contains(item_id))
            });
            let has_cross_volume_candidate = receipt_by_item.iter().any(|(item_id, item)| {
                item.outcome == super::model::ItemOutcome::Completed
                    && !duplicate_items.contains(item_id)
                    && self.move_undo_candidates.get(&(*token, *item_id)).is_some_and(|candidate| candidate.cross_volume_file.as_ref().is_some_and(|proof| proof.directory_manifest.is_none()))
            });
            let has_cross_volume_directory_candidate = receipt_by_item.iter().any(|(item_id, item)| {
                item.outcome == super::model::ItemOutcome::Completed
                    && !duplicate_items.contains(item_id)
                    && self.move_undo_candidates.get(&(*token, *item_id)).is_some_and(|candidate| candidate.cross_volume_file.as_ref().is_some_and(|proof| proof.directory_manifest.is_some()))
            });
            let mut admitted = Vec::new();
            let mut unsupported_cross_group = false;
            for index in 0..job.request.sources.len() {
                let item_id = super::model::ItemId(index as u32);
                if duplicate_items.contains(&item_id) { continue; }
                let Some(item) = receipt_by_item.get(&item_id).copied() else { continue };
                if item.outcome != super::model::ItemOutcome::Completed { continue; }
                let Some(candidate) = self.move_undo_candidates.remove(&(*token, item_id)) else {
                    if has_cross_volume_candidate { unsupported_cross_group = true; }
                    continue;
                };
                let Some(receipt) = item.move_receipt.as_ref() else {
                    if has_cross_volume_candidate { unsupported_cross_group = true; }
                    continue;
                };
                if receipt.destination_published && receipt.source_removed
                    && receipt.source == candidate.source && receipt.destination == candidate.destination {
                    if has_cross_volume_candidate && candidate.cross_volume_file.is_none() { unsupported_cross_group = true; }
                    admitted.push(candidate);
                } else if has_cross_volume_candidate { unsupported_cross_group = true; }
            }
            if job.request.move_destination_identity_guard.is_some()
                && (has_cross_volume_candidate || has_cross_volume_directory_candidate)
            {
                let exact_clipboard_cross_volume_batch = !job.request.sources.is_empty()
                    && report.outcome == super::model::TerminalOutcome::Succeeded
                    && report.receipts.len() == job.request.sources.len()
                    && seen_items.len() == job.request.sources.len()
                    && completed_items == job.request.sources.len()
                    && admitted.len() == job.request.sources.len()
                    && retry_authority.is_none()
                    && !completed_outside_retry
                    && !unsupported_cross_group
                    && !(0..job.request.sources.len()).any(|index| self.invalid_move_undo_candidates.contains(&(*token, super::model::ItemId(index as u32))))
                    && report.receipts.iter().all(|item| {
                        (item.item.0 as usize) < job.request.sources.len()
                            && item.outcome == super::model::ItemOutcome::Completed
                            && item.replace_receipt.is_none()
                            && item.move_receipt.as_ref().is_some_and(|proof| proof.destination_published && proof.source_removed)
                    })
                    && admitted.iter().enumerate().all(|(index, candidate)| {
                        let Some(proof) = candidate.cross_volume_file.as_ref() else { return false };
                        candidate.source == job.request.sources[index]
                            && candidate.destination.parent() == job.request.destination.as_deref()
                            && (candidate.destination.file_name() == job.request.sources[index].file_name()
                                || super::model::is_keep_both_candidate_path(
                                    &job.request.destination.as_ref().unwrap().join(
                                        job.request.sources[index].file_name().unwrap_or_default(),
                                    ),
                                    &candidate.destination,
                                ))
                            && candidate.destination_parent_route.last().is_some_and(|route| {
                                Some(&route.path) == job.request.destination.as_ref()
                                    && Some(&route.identity) == job.request.move_destination_identity_guard.as_ref()
                            })
                            && proof.directory_manifest.is_some() == (candidate.object_kind == super::model::MoveUndoObjectKind::Directory)
                    });
                if exact_clipboard_cross_volume_batch { Some(admitted) } else { None }
            } else if job.request.move_destination_identity_guard.is_some()
                && !has_cross_volume_candidate
                && !has_cross_volume_directory_candidate
            {
                let exact_clipboard_batch = !job.request.sources.is_empty()
                    && report.outcome == super::model::TerminalOutcome::Succeeded
                    && report.receipts.len() == job.request.sources.len()
                    && seen_items.len() == job.request.sources.len()
                    && completed_items == job.request.sources.len()
                    && admitted.len() == job.request.sources.len()
                    && retry_authority.is_none()
                    && !completed_outside_retry
                    && !has_cross_volume_candidate
                    && !has_cross_volume_directory_candidate
                    && !unsupported_cross_group
                    && !(0..job.request.sources.len()).any(|index| self.invalid_move_undo_candidates.contains(&(*token, super::model::ItemId(index as u32))))
                    && report.receipts.iter().all(|item| {
                        (item.item.0 as usize) < job.request.sources.len()
                            && item.outcome == super::model::ItemOutcome::Completed
                            && item.replace_receipt.is_none()
                            && item.move_receipt.as_ref().is_some_and(|proof| proof.destination_published && proof.source_removed)
                    })
                    && admitted.iter().enumerate().all(|(index, candidate)| {
                        candidate.source == job.request.sources[index]
                            && (matches!(candidate.object_kind, super::model::MoveUndoObjectKind::RegularFile | super::model::MoveUndoObjectKind::Directory)
                                || job.request.sources.len() == 1
                                    && matches!(candidate.object_kind, super::model::MoveUndoObjectKind::ReparsePoint { .. }))
                            && candidate.cross_volume_file.is_none()
                            && candidate.destination.parent() == job.request.destination.as_deref()
                            && (candidate.destination.file_name() == job.request.sources[index].file_name()
                                || super::model::is_keep_both_candidate_path(
                                    &job.request.destination.as_ref().unwrap().join(
                                        job.request.sources[index].file_name().unwrap_or_default(),
                                    ),
                                    &candidate.destination,
                                ))
                            && candidate.destination_parent_route.last().is_some_and(|route| {
                                Some(&route.path) == job.request.destination.as_ref()
                                    && Some(&route.identity) == job.request.move_destination_identity_guard.as_ref()
                            })
                    });
                if exact_clipboard_batch { Some(admitted) } else { None }
            } else if has_cross_volume_directory_candidate {
                let exact_directory_group = report.receipts.len() == seen_items.len()
                    && admitted.len() == completed_items
                    && !admitted.is_empty()
                    && admitted.iter().all(|candidate| candidate.cross_volume_file.as_ref().is_some_and(|proof| proof.directory_manifest.is_some()))
                    && !has_cross_volume_candidate
                    && retry_authority.is_none()
                    && !completed_outside_retry
                    && !unsupported_cross_group;
                if exact_directory_group { Some(admitted) } else { None }
            } else if has_cross_volume_candidate || retry_authority.is_some() {
                let clipboard_singleton_succeeded = job.request.move_destination_identity_guard.is_none()
                    || (job.request.sources.len() == 1
                        && report.outcome == super::model::TerminalOutcome::Succeeded
                        && completed_items == 1);
                let exact_completed = report.receipts.iter().all(|item| {
                    if item.outcome != super::model::ItemOutcome::Completed { return true; }
                    !duplicate_items.contains(&item.item)
                        && (item.item.0 as usize) < job.request.sources.len()
                        && item.move_receipt.as_ref().is_some_and(|receipt| receipt.destination_published && receipt.source_removed)
                        && item.move_receipt.is_some()
                }) && report.receipts.len() == seen_items.len()
                    && admitted.len() == completed_items
                    && admitted.iter().all(|candidate| candidate.cross_volume_file.as_ref().is_some_and(|proof| proof.directory_manifest.is_none()))
                    && !completed_outside_retry
                    && clipboard_singleton_succeeded
                    && !unsupported_cross_group;
                if exact_completed && !admitted.is_empty() { Some(admitted) } else { None }
            } else {
                (!admitted.is_empty()).then_some(admitted)
            }
        });
        self.move_undo_candidates.retain(|(candidate_token, _), _| candidate_token != token);
        self.invalid_move_undo_candidates.retain(|(candidate_token, _)| candidate_token != token);
        if let Some(receipts) = move_undo {
            self.undo_ledger.push(UndoEntry::Move(receipts));
            self.trim_undo_ledger();
        }
        // 取消後も完了済みitemの証拠をterminalで判定できるよう、ここで一括清掃する。
        self.keep_both_directory_items
            .retain(|(attempt, _)| attempt != token);
        if let Some(pending) = self.pending_batch_undo.remove(token) {
            let PendingBatchUndo {
                association,
                must_still_be_current,
            } = pending;
            let BatchUndoAssociation {
                receipt_id,
                targets: original,
            } = association;
            let plan = self
                .service
                .job(token.job_id())
                .and_then(|job| job.request.batch_rename_plan);
            let retained = plan
                .as_deref()
                .map(|plan| retained_batch_undo_targets(&original, plan, &report.receipts))
                .unwrap_or(original.clone());
            let prior_receipt = self.undo_ledger.iter().position(|entry| {
                matches!(entry, UndoEntry::BatchRename { receipt_id: current_id, targets }
                    if *current_id == receipt_id && *targets == original)
            });
            let receipt_is_current = self.undo_ledger.last().is_some_and(|entry| {
                matches!(entry, UndoEntry::BatchRename { receipt_id: current_id, targets }
                    if *current_id == receipt_id && *targets == original)
            });
            if must_still_be_current && !receipt_is_current {
                self.batch_undo_receipts.remove(&token.job_id());
            } else {
                match (prior_receipt, retained.is_empty()) {
                    (Some(index), true) => {
                        self.undo_ledger.remove(index);
                    }
                    (Some(index), false) => {
                        self.undo_ledger[index] = UndoEntry::BatchRename {
                            receipt_id,
                            targets: retained.clone(),
                        };
                    }
                    (None, false) if !must_still_be_current => {
                        self.undo_ledger.push(UndoEntry::BatchRename {
                            receipt_id,
                            targets: retained.clone(),
                        });
                        self.trim_undo_ledger();
                    }
                    (None, _) => {}
                }
                if retained.is_empty() {
                    self.batch_undo_receipts.remove(&token.job_id());
                } else {
                    self.batch_undo_receipts.insert(
                        token.job_id(),
                        BatchUndoAssociation {
                            receipt_id,
                            targets: retained,
                        },
                    );
                }
            }
        }
        if let Some(job) = self.service.job(token.job_id())
            && job.request.kind == super::model::OperationKind::BatchRename
            && let Some(plan) = job.request.batch_rename_plan
        {
            let completed: Vec<_> = report
                .receipts
                .iter()
                .filter(|receipt| receipt.outcome == super::model::ItemOutcome::Completed)
                .filter_map(|receipt| plan.get(receipt.item.0 as usize).cloned())
                .collect();
            if !completed.is_empty() {
                self.undo_ledger.push(UndoEntry::BatchRename {
                    receipt_id: BatchRenameUndoId(token.job_id()),
                    targets: completed,
                });
                self.trim_undo_ledger();
            }
        }
        let is_extract = self
            .service
            .job(token.job_id())
            .is_some_and(|job| job.request.kind == super::model::OperationKind::Extract7z);
        if is_extract {
            self.seven_zip_extract_batches.entry(*token).or_default();
            let frozen = self.journal_attempt(*token);
            if let Some(state) = self.seven_zip_extract_batches.get_mut(token) {
                state.frozen_terminal_record = frozen;
            }
        }
        if self.running == Some(*token) {
            self.running = None;
        }
        TerminalCommit { event }
    }

    pub fn acknowledge_seven_zip_extract_terminal_record(
        &mut self,
        token: AttemptToken,
        record: &super::journal::JournalAttempt,
    ) -> bool {
        let Some(state) = self.seven_zip_extract_batches.get(&token) else {
            return false;
        };
        if state.frozen_terminal_record.as_ref() != Some(record)
            || state.failed.is_some()
            || state.pending.is_some()
        {
            return false;
        }
        #[cfg(test)]
        if state.reject_terminal_ack_once {
            let state = self.seven_zip_extract_batches.get_mut(&token).unwrap();
            state.reject_terminal_ack_once = false;
            state.terminal_ack_rejection_consumed = true;
            return false;
        }
        self.release_seven_zip_extract_terminal_delivery(token, record, true)
    }

    pub fn abandon_seven_zip_extract_terminal_delivery(
        &mut self,
        token: AttemptToken,
        record: &super::journal::JournalAttempt,
    ) -> bool {
        self.release_seven_zip_extract_terminal_delivery(token, record, false)
    }

    #[cfg(test)]
    pub(crate) fn send_control_only_cancel_for_test(&self, token: AttemptToken) -> bool {
        self.controls
            .get(&token)
            .is_some_and(|sender| sender.send(WorkerControl::Cancel).is_ok())
    }

    #[cfg(test)]
    pub(crate) fn reject_terminal_ack_once_for_test(&mut self, token: AttemptToken) -> bool {
        let Some(state) = self.seven_zip_extract_batches.get_mut(&token) else {
            return false;
        };
        state.reject_terminal_ack_once = true;
        state.terminal_ack_rejection_consumed = false;
        true
    }

    #[cfg(test)]
    pub(crate) fn terminal_ack_rejection_was_consumed_for_test(&self, token: AttemptToken) -> bool {
        self.seven_zip_extract_batches
            .get(&token)
            .is_some_and(|state| state.terminal_ack_rejection_consumed)
    }

    fn release_seven_zip_extract_terminal_delivery(
        &mut self,
        token: AttemptToken,
        record: &super::journal::JournalAttempt,
        durable: bool,
    ) -> bool {
        let batch = record.seven_zip_extract_batch.clone();
        let Some(state) = self.seven_zip_extract_batches.get_mut(&token) else {
            return false;
        };
        if state.frozen_terminal_record.as_ref() != Some(record)
            || state.terminal_journal_acknowledged
        {
            return false;
        }
        if durable {
            if state.failed.is_some() || state.pending.is_some() {
                return false;
            }
            state.terminal_journal_acknowledged = true;
            if !matches!(
                record.terminal_outcome,
                Some(
                    super::journal::JournalTerminalOutcome::Partial
                        | super::journal::JournalTerminalOutcome::Unknown
                )
            ) {
                state.pending = None;
                state.pending_record = None;
                state.pending_write_claimed = false;
                state.failed = None;
                state.failed_record = None;
                state.durable = None;
                state.frozen_terminal_record = None;
            }
        }
        if !state.terminal_delivery_closed {
            state.terminal_delivery_closed = true;
            if let Some(sender) = self.controls.remove(&token) {
                if durable {
                    let _ = sender.send(WorkerControl::SevenZipExtractTerminalJournaledExact {
                        token,
                        batch,
                    });
                }
            }
        } else if durable {
            // A later persistence retry can release clean retained facts, but it
            // cannot recreate the worker's one-shot reply route.
            self.controls.remove(&token);
        }
        true
    }

    /// serial writer が terminal snapshot を保存するまで journal facts を保持する。
    pub fn acknowledge_terminal_journal(&mut self, token: AttemptToken) -> bool {
        if self
            .job(token)
            .is_some_and(|job| job.request.kind == super::model::OperationKind::Extract7z)
        {
            return false;
        }
        self.release_terminal_journal(token)
    }

    fn release_terminal_journal(&mut self, token: AttemptToken) -> bool {
        let Some(job) = self.job(token) else {
            return false;
        };
        let Some(attempt) = job.attempts.iter().find(|attempt| attempt.token == token) else {
            return false;
        };
        if !matches!(attempt.state, super::model::AttemptState::Terminal)
            || !matches!(
                job.request.kind,
                super::model::OperationKind::ExtractZip
                    | super::model::OperationKind::Extract7z
                    | super::model::OperationKind::Create7z
            )
        {
            return false;
        }
        if job.request.kind == super::model::OperationKind::ExtractZip {
            self.zip_extracts
                .retain(|(attempt, _), _| *attempt != token);
        } else if job.request.kind == super::model::OperationKind::Extract7z {
            let uncertain = attempt.terminal.as_ref().is_some_and(|report| {
                matches!(
                    report.outcome,
                    TerminalOutcome::Partial | TerminalOutcome::Unknown
                )
            });
            if !uncertain {
                let state = self.seven_zip_extract_batches.entry(token).or_default();
                state.pending = None;
                state.pending_record = None;
                state.pending_write_claimed = false;
                state.failed = None;
                state.failed_record = None;
                state.durable = None;
                state.frozen_terminal_record = None;
                state.terminal_journal_acknowledged = true;
            }
        } else if !attempt.terminal.as_ref().is_some_and(|report| {
            matches!(
                report.outcome,
                TerminalOutcome::Partial | TerminalOutcome::Unknown
            )
        }) {
            self.seven_zip_creates.remove(&token);
            self.failed_seven_zip_create_journal.remove(&token);
            self.seven_zip_create_journal_failed.remove(&token);
        }
        true
    }

    fn trim_undo_ledger(&mut self) {
        let excess = self.undo_ledger.len().saturating_sub(UNDO_TRASH_LIMIT);
        if excess > 0 {
            self.undo_ledger.drain(0..excess);
        }
    }
}

fn directory_copy_inspection_evidence(
    terminal: &super::model::TerminalReport,
) -> Vec<super::model::CopyDirectoryInspectionEvidence> {
    let mut found = Vec::new();
    for receipt in &terminal.receipts {
        let Some(detail) = receipt.detail.as_deref() else { continue };
        if let Some((_, payload)) = detail.split_once(super::model::DIRECTORY_COPY_EVIDENCE_PREFIX) {
            if let Ok(evidence) = serde_json::from_str::<super::model::CopyDirectoryPublicationEvidence>(payload) {
                found.push(super::model::CopyDirectoryInspectionEvidence::Publication(evidence));
            }
            continue;
        }
        if let Some((_, payload)) = detail.split_once(super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX) {
            if let Ok(evidence) = serde_json::from_str::<super::model::CopyDirectoryStageEvidence>(payload) {
                found.push(super::model::CopyDirectoryInspectionEvidence::RetainedStage(evidence));
            }
        }
    }
    found
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceCleanupSubmitError {
    InvalidLease,
    Rejected(SubmitError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::model::{
        AttemptState, FsChangeHint, ItemId, ItemOutcome, ItemReceipt, MoveIdentityGuard,
        OperationKind, ProgressCounters, ProgressUpdate, RetryError, RetryPlan, TerminalOutcome,
        TerminalReport,
    };
    use std::path::PathBuf;

    fn request(name: &str) -> OperationRequest {
        OperationRequest {
            kind: OperationKind::Copy,
            virtual_source: None,
            sources: vec![PathBuf::from(name)],
            destination: Some(PathBuf::from("target")),
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
            batch_rename_plan: None,
        }
    }

    #[test]
    fn clipboard_symbolic_link_request_keeps_its_unguarded_shape() {
        let mut request = request("clipboard-source.txt");
        request.kind = OperationKind::CreateSymbolicLink;
        request.origin = Some(super::super::model::OperationOrigin {
            view_id: 1,
            location_epoch: 2,
            presentation_revision: 3,
        });
        let mut runtime = OperationRuntime::default();
        assert!(runtime.submit(request).is_ok());
    }

    fn seven_zip_runtime_fixture() -> (
        tempfile::TempDir,
        OperationRuntime,
        AttemptToken,
        WorkerStart,
        super::super::journal::JournalSevenZipHandleCreate,
    ) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("root.txt");
        std::fs::write(&source, b"captured source").unwrap();
        let output = root.path().join("result.7z");
        let source_guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let provider_hint = |name: &str, index| crate::archive_provider::ArchiveFileHint {
            path: root.path().join(name),
            identity: crate::archive_provider::ArchiveFileIdentity {
                volume_serial: 1,
                file_index: index,
            },
        };
        let request = OperationRequest::create_7z(
            vec![source.clone()],
            output.clone(),
            crate::operations::model::SevenZipCreateGuard {
                destination_parent_identity: source_guard.destination_parent_identity.clone(),
                sources: vec![crate::operations::model::SevenZipCreateSourceGuard {
                    source_identity: source_guard.source_identity.clone(),
                    source_kind: source_guard.source_kind,
                    entry_name: source_guard.entry_name.clone(),
                }],
                provider_revision: 42,
                provider_binding: crate::archive_provider::ArchiveOperationBinding {
                    executable: provider_hint("7z.exe", 2),
                    zip_dll: provider_hint("7z.dll", 3),
                },
            },
        )
        .unwrap();
        let mut ledger = super::super::journal::seven_zip_handle_create_test_fixture();
        ledger.sources[0].path = super::super::journal::JournalPath::from_path(&source);
        let (volume, file_index) = source_guard.source_identity.windows_parts().unwrap();
        ledger.sources[0].identity =
            super::super::journal::JournalSevenZipHandleIdentity { volume, file_index };
        ledger.sources[0].entry_name = source_guard.entry_name.clone();
        ledger.sources[0].directory = false;
        ledger.sources[0].length = std::fs::metadata(&source).unwrap().len();
        ledger.manifest[0].source_identity = Some(ledger.sources[0].identity.clone());
        ledger.manifest[0].name = source_guard.entry_name.clone();
        ledger.manifest[0].length = ledger.sources[0].length;
        ledger.output = super::super::journal::JournalPath::from_path(&output);
        ledger.candidate.path = super::super::journal::JournalPath::from_path(
            &root.path().join("result.candidate.tmp"),
        );
        ledger.stage.path =
            super::super::journal::JournalPath::from_path(&root.path().join("result.stage.tmp"));
        ledger.output_parent_identity = format!("{:?}", source_guard.destination_parent_identity);
        ledger.provider_revision = 42;
        ledger.executable =
            super::super::journal::JournalPath::from_path(&root.path().join("7z.exe"));
        ledger.executable_identity = format!("{:?}", provider_hint("7z.exe", 2).identity);
        ledger.seven_zip_dll =
            super::super::journal::JournalPath::from_path(&root.path().join("7z.dll"));
        ledger.seven_zip_dll_identity = format!("{:?}", provider_hint("7z.dll", 3).identity);
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request.clone()).unwrap();
        let worker = runtime.start_next().unwrap();
        (root, runtime, token, worker, ledger)
    }

    #[test]
    fn seven_zip_initial_intent_accepts_one_root_association_with_nested_file_facts() {
        use super::super::journal::{
            JournalSevenZipHandleEntry as Entry, JournalSevenZipHandleIdentity as Identity,
            JournalSevenZipHandleSource as Source,
        };
        use sha2::Digest;

        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("root.txt");
        std::fs::write(&file, b"root bytes").unwrap();
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let nested = folder.join("nested.txt");
        std::fs::write(&nested, b"nested bytes").unwrap();
        let output = root.path().join("result.7z");

        let file_facts = crate::operations::model::test_zip_create_guard_for_kind(
            &file,
            &output,
            crate::directory::EntryKind::File,
        );
        let folder_facts = crate::operations::model::test_zip_create_guard_for_kind(
            &folder,
            &output,
            crate::directory::EntryKind::Folder,
        );
        let parent_hint = |name: &str, index| crate::archive_provider::ArchiveFileHint {
            path: root.path().join(name),
            identity: crate::archive_provider::ArchiveFileIdentity {
                volume_serial: 1,
                file_index: index,
            },
        };
        let exe = parent_hint("7z.exe", 2);
        let dll = parent_hint("7z.dll", 3);
        let binding = crate::archive_provider::ArchiveOperationBinding {
            executable: exe.clone(),
            zip_dll: dll.clone(),
        };
        let request = OperationRequest::create_7z(
            vec![file.clone(), folder.clone()],
            output.clone(),
            super::super::model::SevenZipCreateGuard {
                destination_parent_identity: file_facts.destination_parent_identity.clone(),
                sources: vec![
                    super::super::model::SevenZipCreateSourceGuard {
                        source_identity: file_facts.source_identity.clone(),
                        source_kind: crate::directory::EntryKind::File,
                        entry_name: "root.txt".into(),
                    },
                    super::super::model::SevenZipCreateSourceGuard {
                        source_identity: folder_facts.source_identity.clone(),
                        source_kind: crate::directory::EntryKind::Folder,
                        entry_name: "folder".into(),
                    },
                ],
                provider_revision: 42,
                provider_binding: binding,
            },
        )
        .unwrap();

        let identity = |path: &std::path::Path| {
            let (volume, file_index) = crate::directory::EntrySourceIdentity::from_path_no_follow(
                path,
                crate::directory::EntryKind::File,
            )
            .unwrap()
            .filesystem_identity()
            .windows_parts()
            .unwrap();
            Identity { volume, file_index }
        };
        let root_file_id = identity(&file);
        let nested_file_id = identity(&nested);
        let (folder_volume, folder_index) = folder_facts.source_identity.windows_parts().unwrap();
        let folder_id = Identity {
            volume: folder_volume,
            file_index: folder_index,
        };
        let source_hash = |bytes: &[u8]| format!("{:x}", sha2::Sha256::digest(bytes));
        let mut ledger = super::super::journal::seven_zip_handle_create_test_fixture();
        ledger.sources = vec![
            Source {
                path: super::super::journal::JournalPath::from_path(&file),
                identity: root_file_id.clone(),
                entry_name: "root.txt".into(),
                directory: false,
                length: b"root bytes".len() as u64,
                sha256: Some(source_hash(b"root bytes")),
            },
            Source {
                path: super::super::journal::JournalPath::from_path(&folder),
                identity: folder_id,
                entry_name: "folder".into(),
                directory: true,
                length: 0,
                sha256: None,
            },
        ];
        ledger.manifest = vec![
            Entry {
                name: "root.txt".into(),
                directory: false,
                source_index: Some(0),
                length: b"root bytes".len() as u64,
                source_identity: Some(root_file_id),
                sha256: Some(source_hash(b"root bytes")),
            },
            Entry {
                name: "folder".into(),
                directory: true,
                source_index: Some(1),
                length: 0,
                source_identity: None,
                sha256: None,
            },
            Entry {
                name: "folder/nested.txt".into(),
                directory: false,
                source_index: Some(1),
                length: b"nested bytes".len() as u64,
                source_identity: Some(nested_file_id),
                sha256: Some(source_hash(b"nested bytes")),
            },
        ];
        ledger.output = super::super::journal::JournalPath::from_path(&output);
        ledger.candidate.path = super::super::journal::JournalPath::from_path(
            &root.path().join("result.candidate.tmp"),
        );
        ledger.stage.path =
            super::super::journal::JournalPath::from_path(&root.path().join("result.stage.tmp"));
        ledger.output_parent_identity = format!("{:?}", file_facts.destination_parent_identity);
        ledger.provider_revision = 42;
        ledger.executable = super::super::journal::JournalPath::from_path(&exe.path);
        ledger.executable_identity = format!("{:?}", exe.identity);
        ledger.seven_zip_dll = super::super::journal::JournalPath::from_path(&dll.path);
        ledger.seven_zip_dll_identity = format!("{:?}", dll.identity);
        ledger.validate().unwrap();

        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request).unwrap();
        let _worker = runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger.clone()),
            "the root association must be admitted while nested files retain their source index"
        );
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
    }

    #[test]
    fn seven_zip_exact_ack_rejects_mismatches_stale_reordered_and_pending_replacement() {
        let (_root, mut runtime, token, worker, ledger) = seven_zip_runtime_fixture();
        for malformed in [
            {
                let mut x = ledger.clone();
                x.provider_revision += 1;
                x
            },
            {
                let mut x = ledger.clone();
                x.output_parent_identity.push('x');
                x
            },
            {
                let mut x = ledger.clone();
                x.sources[0].identity.file_index += 1;
                x
            },
            {
                let mut x = ledger.clone();
                x.manifest[0].name = "evil.txt".into();
                x
            },
        ] {
            runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
                token,
                ledger: malformed,
            });
            assert!(
                runtime
                    .journal_attempt(token)
                    .unwrap()
                    .seven_zip_create_handles
                    .is_none()
            );
        }
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        let wrong_token = runtime.submit(request("wrong-token")).unwrap();
        assert!(!runtime.acknowledge_seven_zip_create_ledger(wrong_token, &ledger));
        let mut replacement = ledger.clone();
        replacement.candidate.path = super::super::journal::JournalPath::from_path(
            &std::path::PathBuf::from(r"C:\temp\replacement"),
        );
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: replacement.clone(),
        });
        assert!(!runtime.acknowledge_seven_zip_create_ledger(token, &replacement));
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger.clone())
        );
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert!(!runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert!(!runtime.acknowledge_seven_zip_create_ledger(token, &ledger));

        let mut next = ledger.clone();
        next.sequence += 1;
        next.candidate.state = super::super::journal::JournalSevenZipHandleObjectState::Owned;
        next.candidate.identity = Some(super::super::journal::JournalSevenZipHandleIdentity {
            volume: 9,
            file_index: 2,
        });
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: next.clone(),
        });
        let mut reordered = next.clone();
        reordered.sequence += 1;
        reordered.phase = super::super::journal::JournalSevenZipHandlePhase::CandidateReady;
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: reordered.clone(),
        });
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(next.clone())
        );
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &next));
        assert!(!runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert!(worker.controls.try_iter().any(|control| matches!(control, WorkerControl::SevenZipCreateLedgerJournaledExact { ledger: ack, .. } if ack == ledger)));
    }

    #[test]
    fn terminal_pending_ledger_ack_with_live_control_is_non_authorizing() {
        let (_root, mut runtime, token, _worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Cancelled,
                final_counters: ProgressCounters {
                    total_items: Some(1),
                    ..ProgressCounters::default()
                },
                receipts: vec![ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            },
        });
        assert_eq!(
            runtime.service.job(token.job_id()).unwrap().attempts[0].state,
            AttemptState::Terminal
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        runtime.controls.insert(token, sender);
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            WorkerControl::SevenZipCreateLedgerJournaledCancelledExact {
                token: ack,
                ledger: exact,
            } if ack == token && exact == ledger
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn seven_zip_cancel_during_writer_wait_sends_non_authorizing_exact_ack() {
        let (_root, mut runtime, token, worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        let _ = runtime.cancel(token);
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert!(worker.controls.try_iter().any(|control| matches!(control, WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { token: ack_token, ledger: ack } if ack_token == token && ack == ledger)));
        let mut progress = ledger.clone();
        progress.sequence += 1;
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: progress,
        });
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger)
        );
    }

    #[test]
    fn cancellation_before_candidate_owned_fact_persists_receipt_but_denies_launch() {
        let (_root, mut runtime, token, worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        let first_ack = worker.controls.try_iter().collect::<Vec<_>>();
        assert!(first_ack.iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledExact { token: ack, ledger: ack_ledger }
                if *ack == token && ack_ledger == &ledger
        )));
        assert!(!first_ack.iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { .. }
        )));
        let _ = runtime.cancel(token);
        let mut owned = ledger.clone();
        owned.sequence += 1;
        owned.candidate.state = super::super::journal::JournalSevenZipHandleObjectState::Owned;
        owned.candidate.identity = Some(super::super::journal::JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 90,
        });
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: owned.clone(),
        });
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(owned.clone())
        );
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &owned));
        let after_owned = worker.controls.try_iter().collect::<Vec<_>>();
        assert!(after_owned.iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { token: ack, ledger }
                if *ack == token && ledger == &owned
        )));
        assert!(!after_owned.iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledExact { ledger, .. } if ledger == &owned
        )));
    }

    #[test]
    fn seven_zip_journal_failure_preserves_unacknowledged_observation_facts() {
        let (_root, mut runtime, token, worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.reject_seven_zip_create_ledger(token, &ledger));
        assert!(worker.controls.try_iter().any(|control| matches!(control, WorkerControl::SevenZipCreateLedgerJournalFailedExact { ledger: failed, .. } if failed == ledger)));
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger.clone())
        );
        let mut next = ledger.clone();
        next.sequence += 1;
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: next,
        });
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger.clone())
        );
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert!(worker.controls.try_iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { token: ack, ledger: late }
                if ack == token && late == ledger
        )));
        let mut cleanup = ledger.clone();
        cleanup.sequence += 1;
        cleanup.candidate.state = super::super::journal::JournalSevenZipHandleObjectState::Owned;
        cleanup.candidate.identity = Some(super::super::journal::JournalSevenZipHandleIdentity {
            volume: 7,
            file_index: 91,
        });
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: cleanup.clone(),
        });
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(cleanup.clone())
        );
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &cleanup));
        let controls = worker.controls.try_iter().collect::<Vec<_>>();
        assert!(controls.iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { token: ack, ledger: exact }
                if *ack == token && exact == &cleanup
        )));
        assert!(!controls.iter().any(|control| matches!(
            control,
            WorkerControl::SevenZipCreateLedgerJournaledExact { ledger: exact, .. }
                if exact == &cleanup
        )));
    }

    #[test]
    fn late_failed_write_ack_keeps_failure_latch_with_closed_control_channel() {
        use super::super::journal::{
            JournalSevenZipHandleIdentity as Identity, JournalSevenZipHandleObjectState as State,
        };
        let (_root, mut runtime, token, worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.reject_seven_zip_create_ledger(token, &ledger));
        drop(worker.controls);
        assert!(!runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger.clone())
        );
        assert!(runtime.seven_zip_create_journal_failed.contains(&token));

        let mut cleanup = ledger.clone();
        cleanup.sequence += 1;
        cleanup.candidate.state = State::Owned;
        cleanup.candidate.identity = Some(Identity {
            volume: 7,
            file_index: 92,
        });
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: cleanup.clone(),
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        runtime.controls.insert(token, sender);
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &cleanup));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            WorkerControl::SevenZipCreateLedgerJournaledCancelledExact {
                token: ack,
                ledger: exact,
            } if ack == token && exact == cleanup
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(cleanup)
        );
        assert!(runtime.seven_zip_create_journal_failed.contains(&token));
    }

    #[test]
    fn seven_zip_success_requires_durable_published_ledger_and_reconciles_receipt() {
        fn claimed_success() -> TerminalReport {
            TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters {
                    processed_items: 1,
                    total_items: Some(1),
                    ..ProgressCounters::default()
                },
                receipts: vec![ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::Completed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: Some(PathBuf::from(r"C:\result.7z")),
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            }
        }

        let (_root, mut runtime, token, _worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.reject_seven_zip_create_ledger(token, &ledger));
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: claimed_success(),
        });
        let failed_write_report = runtime
            .service
            .job(token.job_id())
            .unwrap()
            .attempts
            .iter()
            .find(|attempt| attempt.token == token)
            .unwrap()
            .terminal
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(failed_write_report.outcome, TerminalOutcome::Unknown);
        assert_eq!(
            failed_write_report.receipts[0].outcome,
            ItemOutcome::Unknown
        );
        assert!(
            failed_write_report.receipts[0]
                .created_archive_path
                .is_none()
        );
        assert_eq!(failed_write_report.final_counters.processed_items, 0);
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .seven_zip_create_handles,
            Some(ledger)
        );

        for discarded in [false, true] {
            let (_root, mut runtime, token, _worker, mut ledger) = seven_zip_runtime_fixture();
            if discarded {
                ledger.phase = super::super::journal::JournalSevenZipHandlePhase::Discarded;
                ledger.aborting = true;
                ledger.candidate.state =
                    super::super::journal::JournalSevenZipHandleObjectState::NeverCreated;
                ledger.candidate.identity = None;
                ledger.stage.state =
                    super::super::journal::JournalSevenZipHandleObjectState::NeverCreated;
                ledger.stage.identity = None;
                runtime.seven_zip_creates.insert(token, ledger);
            }
            runtime.commit_fact(ExecutorFact::Terminal {
                token,
                report: claimed_success(),
            });
            let report = runtime
                .service
                .job(token.job_id())
                .unwrap()
                .attempts
                .iter()
                .find(|attempt| attempt.token == token)
                .unwrap()
                .terminal
                .as_ref()
                .unwrap()
                .clone();
            assert_eq!(report.outcome, TerminalOutcome::Unknown);
            assert_eq!(report.receipts[0].outcome, ItemOutcome::Unknown);
        }
    }

    #[test]
    fn seven_zip_cancel_is_clean_only_before_intent_or_after_durable_discard() {
        fn cancelled() -> TerminalReport {
            TerminalReport {
                outcome: TerminalOutcome::Cancelled,
                final_counters: ProgressCounters {
                    total_items: Some(1),
                    ..ProgressCounters::default()
                },
                receipts: vec![ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            }
        }

        let (_root, mut runtime, token, _worker, _ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: cancelled(),
        });
        assert_eq!(
            runtime.service.job(token.job_id()).unwrap().attempts[0]
                .terminal
                .as_ref()
                .unwrap()
                .outcome,
            TerminalOutcome::Cancelled
        );
        assert_eq!(
            runtime.service.job(token.job_id()).unwrap().attempts[0]
                .terminal
                .as_ref()
                .unwrap()
                .receipts[0]
                .outcome,
            ItemOutcome::CancelledBeforeStart
        );

        let (_root, mut runtime, token, _worker, ledger) = seven_zip_runtime_fixture();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: cancelled(),
        });
        let attempt = &runtime.service.job(token.job_id()).unwrap().attempts[0];
        assert_eq!(
            attempt.terminal.as_ref().unwrap().outcome,
            TerminalOutcome::Unknown
        );
        assert_eq!(
            attempt.terminal.as_ref().unwrap().receipts[0].outcome,
            ItemOutcome::Unknown
        );

        let (_root, mut runtime, token, _worker, mut ledger) = seven_zip_runtime_fixture();
        use super::super::journal::{
            JournalSevenZipHandleObjectState as State, JournalSevenZipHandlePhase as Phase,
        };
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
        for (candidate, stage, phase) in [
            (
                State::NeverCreated,
                State::Planned,
                Phase::CandidateCreateIntent,
            ),
            (
                State::NeverCreated,
                State::NeverCreated,
                Phase::CandidateCreateIntent,
            ),
            (State::NeverCreated, State::NeverCreated, Phase::Discarded),
        ] {
            let mut next = ledger.clone();
            next.sequence += 1;
            next.candidate.state = candidate;
            next.stage.state = stage;
            next.phase = phase;
            next.aborting = true;
            assert!(next.follows(&ledger));
            runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
                token,
                ledger: next.clone(),
            });
            assert!(runtime.acknowledge_seven_zip_create_ledger(token, &next));
            ledger = next;
        }
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: cancelled(),
        });
        assert_eq!(
            runtime.service.job(token.job_id()).unwrap().attempts[0]
                .terminal
                .as_ref()
                .unwrap()
                .outcome,
            TerminalOutcome::Cancelled
        );
    }

    #[test]
    fn seven_zip_non_success_outcomes_match_each_durable_ownership_phase() {
        use super::super::journal::{
            JournalSevenZipHandleIdentity as Identity, JournalSevenZipHandleObjectState as State,
            JournalSevenZipHandlePhase as Phase,
        };
        fn acknowledge(
            runtime: &mut OperationRuntime,
            token: AttemptToken,
            current: &mut super::super::journal::JournalSevenZipHandleCreate,
            mut next: super::super::journal::JournalSevenZipHandleCreate,
        ) {
            next.sequence = current.sequence.checked_add(1).unwrap();
            assert!(next.follows(current), "invalid phase fixture: {next:?}");
            next.validate().unwrap();
            runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
                token,
                ledger: next.clone(),
            });
            assert!(runtime.acknowledge_seven_zip_create_ledger(token, &next));
            *current = next;
        }

        fn ledger_at(
            runtime: &mut OperationRuntime,
            token: AttemptToken,
            mut ledger: super::super::journal::JournalSevenZipHandleCreate,
            phase: Phase,
        ) -> super::super::journal::JournalSevenZipHandleCreate {
            runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
                token,
                ledger: ledger.clone(),
            });
            assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));
            if phase == Phase::Discarded {
                let mut next = ledger.clone();
                next.aborting = true;
                next.candidate.state = State::NeverCreated;
                acknowledge(runtime, token, &mut ledger, next);
                let mut next = ledger.clone();
                next.stage.state = State::NeverCreated;
                acknowledge(runtime, token, &mut ledger, next);
                let mut next = ledger.clone();
                next.phase = Phase::Discarded;
                acknowledge(runtime, token, &mut ledger, next);
                return ledger;
            }
            if phase == Phase::CandidateCreateIntent {
                return ledger;
            }
            let mut next = ledger.clone();
            next.candidate.state = State::Owned;
            next.candidate.identity = Some(Identity {
                volume: 7,
                file_index: 20,
            });
            acknowledge(runtime, token, &mut ledger, next);
            let mut next = ledger.clone();
            next.phase = Phase::CandidateReady;
            next.candidate.length = Some(15);
            acknowledge(runtime, token, &mut ledger, next);
            if phase == Phase::CandidateReady {
                return ledger;
            }
            let mut next = ledger.clone();
            next.phase = Phase::StageCreateIntent;
            next.stage.state = State::CreateIntent;
            acknowledge(runtime, token, &mut ledger, next);
            if phase == Phase::StageCreateIntent {
                return ledger;
            }
            let mut next = ledger.clone();
            next.stage.state = State::Owned;
            next.stage.identity = Some(Identity {
                volume: 7,
                file_index: 21,
            });
            acknowledge(runtime, token, &mut ledger, next);
            let mut next = ledger.clone();
            next.phase = Phase::StageReady;
            next.stage.length = Some(15);
            acknowledge(runtime, token, &mut ledger, next);
            if phase == Phase::StageReady {
                return ledger;
            }
            let mut next = ledger.clone();
            next.candidate.state = State::DeleteIntent;
            acknowledge(runtime, token, &mut ledger, next);
            let mut next = ledger.clone();
            next.candidate.state = State::Deleted;
            acknowledge(runtime, token, &mut ledger, next);
            let mut next = ledger.clone();
            next.phase = Phase::CandidateCleaned;
            acknowledge(runtime, token, &mut ledger, next);
            if phase == Phase::CandidateCleaned {
                return ledger;
            }
            let mut next = ledger.clone();
            next.phase = Phase::PublishIntent;
            acknowledge(runtime, token, &mut ledger, next);
            if phase == Phase::PublishIntent {
                return ledger;
            }
            let mut next = ledger.clone();
            next.phase = Phase::Published;
            next.stage.state = State::Published;
            next.archive_identity = next.stage.identity.clone();
            acknowledge(runtime, token, &mut ledger, next);
            ledger
        }

        let phases = [
            Phase::CandidateCreateIntent,
            Phase::CandidateReady,
            Phase::StageCreateIntent,
            Phase::StageReady,
            Phase::CandidateCleaned,
            Phase::PublishIntent,
            Phase::Published,
            Phase::Discarded,
        ];
        let outcomes = [
            TerminalOutcome::Failed,
            TerminalOutcome::Cancelled,
            TerminalOutcome::Unknown,
            TerminalOutcome::Partial,
        ];
        for phase in phases {
            for outcome in outcomes {
                let (root, mut runtime, token, _worker, initial) = seven_zip_runtime_fixture();
                let ledger = ledger_at(&mut runtime, token, initial, phase);
                let report = TerminalReport {
                    outcome,
                    final_counters: ProgressCounters {
                        processed_items: 1,
                        processed_bytes: 15,
                        total_items: Some(1),
                        ..ProgressCounters::default()
                    },
                    receipts: vec![ItemReceipt {
                        item: ItemId(0),
                        outcome: ItemOutcome::Completed,
                        detail: None,
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    }],
                    retry: RetryPlan::NotAllowed,
                    change_hints: Vec::new(),
                };
                runtime.commit_fact(ExecutorFact::Terminal { token, report });
                let job = runtime.service.job(token.job_id()).unwrap();
                let actual = job.attempts[0].terminal.as_ref().unwrap();
                if phase == Phase::Published {
                    assert_eq!(actual.outcome, TerminalOutcome::Partial);
                    assert_eq!(
                        actual.receipts[0].created_archive_path,
                        Some(root.path().join("result.7z"))
                    );
                    assert_eq!(actual.receipts[0].outcome, ItemOutcome::Partial);
                } else {
                    let expected = match (phase, outcome) {
                        (
                            Phase::Discarded,
                            TerminalOutcome::Failed | TerminalOutcome::Cancelled,
                        ) => outcome,
                        (_, TerminalOutcome::Failed | TerminalOutcome::Cancelled) => {
                            TerminalOutcome::Unknown
                        }
                        (_, outcome) => outcome,
                    };
                    assert_eq!(
                        actual.outcome, expected,
                        "phase={phase:?} outcome={outcome:?}"
                    );
                    if expected == TerminalOutcome::Unknown {
                        assert_eq!(actual.receipts[0].outcome, ItemOutcome::Unknown);
                    } else if expected == TerminalOutcome::Partial {
                        assert_eq!(actual.receipts[0].outcome, ItemOutcome::Partial);
                    }
                    assert!(
                        actual.receipts[0].created_archive_path.is_none(),
                        "phase={phase:?} outcome={outcome:?}"
                    );
                }
                assert_eq!(ledger.phase, phase);
            }
        }
    }

    #[test]
    fn seven_zip_durable_publication_accepts_only_its_exact_single_item_success_report() {
        fn published(
            mut ledger: super::super::journal::JournalSevenZipHandleCreate,
        ) -> super::super::journal::JournalSevenZipHandleCreate {
            use super::super::journal::{
                JournalSevenZipHandleIdentity as Identity,
                JournalSevenZipHandleObjectState as State, JournalSevenZipHandlePhase as Phase,
            };
            ledger.sequence = 20;
            ledger.phase = Phase::Published;
            ledger.aborting = false;
            ledger.candidate.state = State::Deleted;
            ledger.candidate.identity = Some(Identity {
                volume: 7,
                file_index: 12,
            });
            ledger.candidate.length = Some(20);
            ledger.stage.state = State::Published;
            ledger.stage.identity = Some(Identity {
                volume: 7,
                file_index: 13,
            });
            ledger.stage.length = Some(20);
            ledger.archive_identity = ledger.stage.identity.clone();
            assert!(ledger.validate().is_ok(), "{:?}", ledger.validate());
            ledger
        }

        fn success_report(output: PathBuf) -> TerminalReport {
            TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters {
                    processed_items: 1,
                    total_items: Some(1),
                    ..ProgressCounters::default()
                },
                receipts: vec![ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::Completed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: Some(output),
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            }
        }

        fn terminal_report(runtime: &OperationRuntime, token: AttemptToken) -> TerminalReport {
            runtime
                .service
                .job(token.job_id())
                .unwrap()
                .attempts
                .iter()
                .find(|attempt| attempt.token == token)
                .unwrap()
                .terminal
                .as_ref()
                .unwrap()
                .clone()
        }

        let (_root, mut runtime, token, _worker, ledger) = seven_zip_runtime_fixture();
        let ledger = published(ledger);
        runtime.seven_zip_creates.insert(token, ledger.clone());
        let output = runtime
            .service
            .job(token.job_id())
            .unwrap()
            .request
            .destination
            .clone()
            .unwrap();
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: success_report(output),
        });
        let accepted = terminal_report(&runtime, token);
        assert_eq!(accepted.outcome, TerminalOutcome::Succeeded);
        assert_eq!(accepted.receipts.len(), 1);
        assert_eq!(accepted.receipts[0].outcome, ItemOutcome::Completed);

        let output = runtime
            .service
            .job(token.job_id())
            .unwrap()
            .request
            .destination
            .clone()
            .unwrap();
        let mut malformed_reports = Vec::new();
        let mut no_receipt = success_report(output.clone());
        no_receipt.receipts.clear();
        malformed_reports.push(no_receipt);
        let mut wrong_item = success_report(output.clone());
        wrong_item.receipts[0].item = ItemId(1);
        malformed_reports.push(wrong_item);
        let mut wrong_output = success_report(output.clone());
        wrong_output.receipts[0].created_archive_path = Some(output.with_file_name("other.7z"));
        malformed_reports.push(wrong_output);
        let mut multiple = success_report(output.clone());
        multiple.receipts.push(multiple.receipts[0].clone());
        malformed_reports.push(multiple);
        let mut wrong_processed = success_report(output.clone());
        wrong_processed.final_counters.processed_items = 0;
        malformed_reports.push(wrong_processed);
        let mut unknown_total = success_report(output.clone());
        unknown_total.final_counters.total_items = None;
        malformed_reports.push(unknown_total);
        let mut unrelated_folder = success_report(output.clone());
        unrelated_folder.receipts[0].created_folder_path = Some(PathBuf::from(r"C:\other"));
        malformed_reports.push(unrelated_folder);
        let mut unrelated_move = success_report(output.clone());
        unrelated_move.receipts[0].move_receipt = Some(super::super::model::MoveReceipt {
            source: PathBuf::from(r"C:\source"),
            destination: output.clone(),
            destination_published: true,
            source_removed: false,
        });
        malformed_reports.push(unrelated_move);

        for report in malformed_reports {
            let (_root, mut runtime, token, _worker, ledger) = seven_zip_runtime_fixture();
            let ledger = published(ledger);
            let output = runtime
                .service
                .job(token.job_id())
                .unwrap()
                .request
                .destination
                .clone()
                .unwrap();
            runtime.seven_zip_creates.insert(token, ledger);
            runtime.commit_fact(ExecutorFact::Terminal { token, report });
            let normalized = terminal_report(&runtime, token);
            assert_eq!(normalized.outcome, TerminalOutcome::Partial);
            assert_eq!(normalized.retry, RetryPlan::RequiresReinspection);
            assert_eq!(normalized.final_counters.processed_items, 1);
            assert_eq!(normalized.final_counters.total_items, Some(1));
            assert_eq!(normalized.receipts.len(), 1);
            assert_eq!(normalized.receipts[0].item, ItemId(0));
            assert_eq!(normalized.receipts[0].outcome, ItemOutcome::Partial);
            assert_eq!(
                normalized.receipts[0].created_archive_path.as_ref(),
                Some(&output)
            );
            assert!(normalized.receipts[0].created_folder_path.is_none());
            assert!(normalized.receipts[0].move_receipt.is_none());
            assert!(normalized.receipts[0].replace_receipt.is_none());
        }
    }

    #[test]
    fn seven_zip_live_published_ledger_roundtrips_through_store_ack_and_terminal_success() {
        use super::super::journal::{
            JournalSevenZipHandleIdentity as Identity, JournalSevenZipHandleObjectState as State,
            JournalSevenZipHandlePhase as Phase, JournalStore,
        };

        fn step(
            runtime: &mut OperationRuntime,
            store: &mut JournalStore,
            token: AttemptToken,
            ledger: &mut super::super::journal::JournalSevenZipHandleCreate,
            next: super::super::journal::JournalSevenZipHandleCreate,
        ) {
            assert!(next.follows(ledger), "invalid ledger transition: {next:?}");
            next.validate().unwrap();
            *ledger = next.clone();
            runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
                token,
                ledger: next.clone(),
            });
            let record = runtime.journal_attempt(token).unwrap();
            assert_eq!(record.seven_zip_create_handles, Some(next.clone()));
            store.upsert(record).unwrap();
            assert!(matches!(
                store.recoveries().as_slice(),
                [super::super::journal::RecoveryItem::RecoveryRequired(attempt)]
                    if attempt.seven_zip_create_handles.as_ref() == Some(&next)
            ));
            assert!(runtime.acknowledge_seven_zip_create_ledger(token, &next));
        }

        let (root, mut runtime, token, _worker, mut ledger) = seven_zip_runtime_fixture();
        let mut store = JournalStore::open(root.path().join("published-roundtrip.json")).unwrap();
        store
            .upsert(runtime.journal_attempt(token).unwrap())
            .unwrap();
        runtime.commit_fact(ExecutorFact::SevenZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        let record = runtime.journal_attempt(token).unwrap();
        assert_eq!(record.seven_zip_create_handles, Some(ledger.clone()));
        store.upsert(record).unwrap();
        assert!(matches!(
            store.recoveries().as_slice(),
            [super::super::journal::RecoveryItem::RecoveryRequired(attempt)]
                if attempt.seven_zip_create_handles.as_ref() == Some(&ledger)
        ));
        assert!(runtime.acknowledge_seven_zip_create_ledger(token, &ledger));

        let mut next = ledger.clone();
        next.sequence += 1;
        next.candidate.state = State::Owned;
        next.candidate.identity = Some(Identity {
            volume: 7,
            file_index: 80,
        });
        step(&mut runtime, &mut store, token, &mut ledger, next);
        assert!(matches!(
            store.recoveries().as_slice(),
            [super::super::journal::RecoveryItem::RecoveryRequired(attempt)]
                if attempt.seven_zip_create_handles.as_ref() == Some(&ledger)
        ));
        let mut next = ledger.clone();
        next.sequence += 1;
        next.phase = Phase::CandidateReady;
        next.candidate.length = Some(15);
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.phase = Phase::StageCreateIntent;
        next.stage.state = State::CreateIntent;
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.stage.state = State::Owned;
        next.stage.identity = Some(Identity {
            volume: 7,
            file_index: 81,
        });
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.phase = Phase::StageReady;
        next.stage.length = Some(15);
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.candidate.state = State::DeleteIntent;
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.candidate.state = State::Deleted;
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.phase = Phase::CandidateCleaned;
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.phase = Phase::PublishIntent;
        step(&mut runtime, &mut store, token, &mut ledger, next);
        let mut next = ledger.clone();
        next.sequence += 1;
        next.phase = Phase::Published;
        next.stage.state = State::Published;
        next.archive_identity = next.stage.identity.clone();
        step(&mut runtime, &mut store, token, &mut ledger, next);
        assert_eq!(store.recoveries().len(), 1);
        assert_eq!(ledger.phase, Phase::Published);

        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters {
                    processed_items: 1,
                    total_items: Some(1),
                    ..ProgressCounters::default()
                },
                receipts: vec![ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::Completed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: Some(root.path().join("result.7z")),
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            },
        });
        let terminal = runtime.journal_attempt(token).unwrap();
        store.upsert(terminal).unwrap();
        assert!(store.recoveries().is_empty());
    }

    #[test]
    fn zip_create_ack_requires_the_exact_current_ledger_and_emits_only_after_it_is_saved() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let second_source = root.path().join("second.txt");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&second_source, b"second").unwrap();
        let output = root.path().join("target.zip");
        let first_guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let second_guard = crate::operations::model::test_zip_create_guard(&second_source, &output);
        let request = OperationRequest::create_zip_many(
            vec![source.clone(), second_source.clone()],
            output.clone(),
            super::super::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity,
                sources: vec![
                    super::super::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    super::super::model::ZipCreateSourceGuard {
                        source_identity: second_guard.source_identity,
                        source_kind: second_guard.source_kind,
                        entry_name: second_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        let crate::operations::model::ZipOperationGuard::Create(guard) =
            request.zip_create_guard.as_ref().unwrap().clone()
        else {
            panic!("create guard")
        };
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let token = runtime.submit(request).unwrap();
        let _worker_command = runtime.start_next().unwrap();
        let ledger = super::super::journal::JournalZipCreate {
            sources: vec![
                super::super::journal::JournalZipCreateSource {
                    source: super::super::journal::JournalPath::from_path(&source),
                    source_identity: format!("{:?}", guard.sources[0].source_identity),
                },
                super::super::journal::JournalZipCreateSource {
                    source: super::super::journal::JournalPath::from_path(&second_source),
                    source_identity: format!("{:?}", guard.sources[1].source_identity),
                },
            ],
            output: super::super::journal::JournalPath::from_path(&output),
            stage: super::super::journal::JournalPath::from_path(&root.path().join("stage.tmp")),
            parent_identity: format!("{:?}", guard.destination_parent_identity),
            stage_identity: None,
            output_identity: None,
            phase: super::super::journal::JournalZipCreatePhase::StagePlanned,
        };
        let mut early_output_identity = ledger.clone();
        early_output_identity.output_identity = Some("too-early-output-id".into());
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: early_output_identity.clone(),
        });
        assert!(runtime.journal_attempt(token).unwrap().zip_create.is_none());
        assert!(!runtime.acknowledge_zip_create_ledger(token, &early_output_identity));
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert_eq!(
            runtime.journal_attempt(token).unwrap().zip_create,
            Some(ledger.clone())
        );

        for malformed in [
            {
                let mut candidate = ledger.clone();
                candidate.sources.swap(0, 1);
                candidate
            },
            {
                let mut candidate = ledger.clone();
                candidate.sources.pop();
                candidate
            },
            {
                let mut candidate = ledger.clone();
                candidate.sources.push(candidate.sources[0].clone());
                candidate
            },
            {
                let mut candidate = ledger.clone();
                candidate.sources[1].source_identity = "malicious-second-id".into();
                candidate
            },
        ] {
            runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
                token,
                ledger: malformed.clone(),
            });
            assert!(!runtime.acknowledge_zip_create_ledger(token, &malformed));
        }

        let mut mismatched = ledger.clone();
        mismatched.output =
            super::super::journal::JournalPath::from_path(&root.path().join("other.zip"));
        assert!(!runtime.acknowledge_zip_create_ledger(token, &mismatched));
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
        assert!(!runtime.acknowledge_zip_create_ledger(token, &ledger));

        let mut ledger = ledger;
        ledger.stage_identity = Some("stage-id".into());
        ledger.phase = super::super::journal::JournalZipCreatePhase::StageOwned;
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
        ledger.phase = super::super::journal::JournalZipCreatePhase::PublishIntent;
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
        ledger.output_identity = Some("published-id".into());
        ledger.phase = super::super::journal::JournalZipCreatePhase::Published;
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        let mut wrong_output_identity = ledger.clone();
        wrong_output_identity.output_identity = Some("different-published-id".into());
        assert!(!runtime.acknowledge_zip_create_ledger(token, &wrong_output_identity));
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));

        let mut terminal = report();
        terminal.final_counters.processed_items = 1;
        terminal.final_counters.discovered_items = 1;
        terminal.final_counters.total_items = Some(1);
        terminal.receipts[0].created_archive_path = Some(output);
        let complete_sources = runtime
            .journal_attempt(token)
            .unwrap()
            .zip_create
            .unwrap()
            .sources;
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: terminal,
            })
            .unwrap();
        let journal = runtime.journal_attempt(token).unwrap();
        assert_eq!(journal.state, super::super::journal::JournalState::Terminal);
        assert_eq!(journal.sources.len(), 2);
        assert_eq!(complete_sources.len(), 2);
        let job = runtime.service.job(token.job_id()).unwrap();
        let terminal = job
            .attempts
            .iter()
            .find(|attempt| attempt.token == token)
            .unwrap()
            .terminal
            .as_ref()
            .unwrap();
        assert_eq!(terminal.final_counters.total_items, Some(1));
        assert_eq!(terminal.receipts.len(), 1);
        assert_eq!(terminal.receipts[0].item, ItemId(0));
    }

    #[test]
    fn zip_create_runtime_admits_only_a_valid_ordered_create_guard() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("one.txt");
        let second = root.path().join("two.txt");
        std::fs::write(&first, b"1").unwrap();
        std::fs::write(&second, b"2").unwrap();
        let output = root.path().join("one.zip");
        let a = crate::operations::model::test_zip_create_guard(&first, &output);
        let b = crate::operations::model::test_zip_create_guard(&second, &output);
        let make_request = || {
            OperationRequest::create_zip_many(
                vec![first.clone(), second.clone()],
                output.clone(),
                super::super::model::ZipArchiveCreateGuard {
                    destination_parent_identity: a.destination_parent_identity.clone(),
                    sources: vec![
                        super::super::model::ZipCreateSourceGuard {
                            source_identity: a.source_identity.clone(),
                            source_kind: a.source_kind,
                            entry_name: a.entry_name.clone(),
                        },
                        super::super::model::ZipCreateSourceGuard {
                            source_identity: b.source_identity.clone(),
                            source_kind: b.source_kind,
                            entry_name: b.entry_name.clone(),
                        },
                    ],
                },
            )
            .unwrap()
        };
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let token = runtime.submit(make_request()).unwrap();
        assert!(runtime.start_next().is_some());
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: report(),
            })
            .unwrap();

        let mut reordered = make_request();
        reordered.sources.swap(0, 1);
        assert_eq!(
            OperationRuntime::new(OperationService::new(1)).submit(reordered),
            Err(SubmitError::InvalidRequest)
        );
        let mut truncated = make_request();
        truncated.sources.pop();
        assert_eq!(
            OperationRuntime::new(OperationService::new(1)).submit(truncated),
            Err(SubmitError::InvalidRequest)
        );
        let mut wrong_variant = make_request();
        wrong_variant.zip_create_guard = Some(super::super::model::ZipOperationGuard::Extract(a));
        assert_eq!(
            OperationRuntime::new(OperationService::new(1)).submit(wrong_variant),
            Err(SubmitError::InvalidRequest)
        );
    }

    #[test]
    fn cancelled_zip_extract_before_first_intent_resolves_planned_and_discard_barriers() {
        use super::super::journal::{
            JournalPath, JournalStore, JournalZipExtract, JournalZipExtractPhase,
        };

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("archive.zip");
        let output = root.path().join("archive");
        std::fs::write(&source, b"zip").unwrap();
        let request = OperationRequest::extract_zip(
            source.clone(),
            output.clone(),
            crate::operations::model::test_zip_create_guard(&source, &output),
        )
        .unwrap();
        let crate::operations::model::ZipOperationGuard::Extract(guard) =
            request.zip_create_guard.as_ref().unwrap().clone()
        else {
            panic!("extract guard")
        };
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request).unwrap();
        let worker = runtime.start_next().unwrap();

        // The worker has not emitted/consumed its first journal fact yet.
        assert!(runtime.cancel(token).is_none());
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        let planned = JournalZipExtract {
            source: JournalPath::from_path(&source),
            output: JournalPath::from_path(&output),
            stage: JournalPath::from_path(&root.path().join("archive.stage")),
            source_identity: format!("{:?}", guard.source_identity),
            parent_identity: format!("{:?}", guard.destination_parent_identity),
            stage_identity: None,
            phase: JournalZipExtractPhase::StagePlanned,
        };
        runtime.commit_fact(ExecutorFact::ZipExtractLedgerIntent {
            token,
            item: super::super::model::ItemId(0),
            ledger: planned.clone(),
        });
        let planned_attempt = runtime.journal_attempt(token).unwrap();
        assert_eq!(
            planned_attempt.state,
            super::super::journal::JournalState::CancelRequested
        );
        store.upsert(planned_attempt).unwrap();
        assert_eq!(
            runtime.journal_attempt(token).unwrap().zip_extract,
            Some(planned.clone())
        );
        assert!(runtime.acknowledge_zip_extract_ledger(token, &planned));
        assert_eq!(worker.controls.try_recv(), Ok(WorkerControl::Cancel));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipExtractLedgerJournaledExact {
                token,
                item: super::super::model::ItemId(0),
                ledger: planned.clone(),
            })
        );

        let mut discarded = planned;
        discarded.phase = JournalZipExtractPhase::StageDiscarded;
        runtime.commit_fact(ExecutorFact::ZipExtractLedgerIntent {
            token,
            item: super::super::model::ItemId(0),
            ledger: discarded.clone(),
        });
        let discarded_attempt = runtime.journal_attempt(token).unwrap();
        store.upsert(discarded_attempt.clone()).unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("journal.json")).unwrap())
                .unwrap();
        let persisted_attempt = &persisted["attempts"][0];
        assert_eq!(persisted_attempt["state"], "cancel-requested");
        assert_eq!(
            persisted_attempt["zip_extract"],
            serde_json::to_value(&discarded).unwrap()
        );
        let mut mutated_identity = discarded_attempt.clone();
        mutated_identity
            .zip_extract
            .as_mut()
            .unwrap()
            .stage_identity = Some("unexpected-stage-identity".into());
        assert!(store.upsert(mutated_identity).is_err());
        let mut mutated_path = discarded_attempt.clone();
        mutated_path.zip_extract.as_mut().unwrap().stage =
            JournalPath::from_path(&root.path().join("other.stage"));
        assert!(store.upsert(mutated_path).is_err());
        let mut publish = discarded_attempt.clone();
        let mut forbidden = discarded.clone();
        forbidden.phase = JournalZipExtractPhase::PublishIntent;
        publish.zip_extract = Some(forbidden);
        assert!(store.upsert(publish).is_err());
        assert_eq!(
            runtime.journal_attempt(token).unwrap().zip_extract,
            Some(discarded.clone())
        );
        assert!(runtime.acknowledge_zip_extract_ledger(token, &discarded));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipExtractLedgerJournaledExact {
                token,
                item: super::super::model::ItemId(0),
                ledger: discarded,
            })
        );
        assert!(!output.exists());
        assert!(!root.path().join("archive.stage").exists());
    }

    #[test]
    fn seven_zip_extract_registration_needs_exact_writer_ack_and_duplicate_cannot_rearm() {
        use super::super::journal::{
            JournalPath, JournalSevenZipExtractAttemptId, JournalSevenZipExtractBatch,
            JournalSevenZipExtractDescriptor, JournalSevenZipExtractEnvelope,
            JournalSevenZipExtractFileBinding, JournalSevenZipExtractIdentity,
            JournalSevenZipExtractLedger, JournalSevenZipExtractObjectState,
            JournalSevenZipExtractPhase, JournalStore, SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
        };
        use super::super::model::{SevenZipExtractGuard, SevenZipExtractSourceGuard};
        let root = tempfile::tempdir().unwrap();
        let sources = [root.path().join("one.7z"), root.path().join("two.7z")];
        for source in &sources {
            std::fs::write(source, b"archive").unwrap();
        }
        let observed: Vec<_> = sources
            .iter()
            .map(|source| {
                crate::operations::model::test_zip_create_guard(source, &source.with_extension(""))
            })
            .collect();
        let parent = crate::operations::model::test_zip_create_guard(
            &sources[0],
            &root.path().join("unused"),
        );
        let binding = crate::archive_provider::ArchiveOperationBinding {
            executable: crate::archive_provider::ArchiveFileHint {
                path: root.path().join("7z.exe"),
                identity: crate::archive_provider::ArchiveFileIdentity {
                    volume_serial: 1,
                    file_index: 800,
                },
            },
            zip_dll: crate::archive_provider::ArchiveFileHint {
                path: root.path().join("7z.dll"),
                identity: crate::archive_provider::ArchiveFileIdentity {
                    volume_serial: 1,
                    file_index: 801,
                },
            },
        };
        let request = OperationRequest::extract_7z_many(
            sources.to_vec(),
            SevenZipExtractGuard {
                destination_parent_identity: parent.destination_parent_identity.clone(),
                sources: observed
                    .iter()
                    .map(|guard| SevenZipExtractSourceGuard {
                        source_identity: guard.source_identity.clone(),
                        source_kind: guard.source_kind,
                        entry_name: guard.entry_name.clone(),
                    })
                    .collect(),
                provider_revision: 9,
                provider_binding: binding.clone(),
            },
        )
        .unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request.clone()).unwrap();
        let worker = runtime.start_next().unwrap();
        let run_id = runtime.run_id.unwrap();
        let id = |identity: &crate::filesystem_identity::FilesystemIdentity| {
            let (volume, file_index) = identity.windows_parts().unwrap();
            JournalSevenZipExtractIdentity { volume, file_index }
        };
        let attempt = JournalSevenZipExtractAttemptId {
            run_id,
            job: token.job_id().value(),
            attempt: token.attempt_id().value(),
        };
        let descriptors = sources
            .iter()
            .enumerate()
            .map(|(ordinal, source)| {
                let output = source.with_extension("");
                JournalSevenZipExtractDescriptor {
                    ordinal: ordinal as u32,
                    source_path: JournalPath::from_path(source),
                    source_identity: id(&observed[ordinal].source_identity),
                    output: JournalPath::from_path(&output),
                    spool: JournalPath::from_path(&root.path().join(format!("{ordinal}.spool"))),
                    tree: JournalPath::from_path(&root.path().join(format!("{ordinal}.tree"))),
                }
            })
            .collect();
        let file_binding = |path: &std::path::Path, identity| JournalSevenZipExtractFileBinding {
            path: JournalPath::from_path(path),
            identity,
        };
        let envelope = JournalSevenZipExtractEnvelope {
            attempt,
            reservation_format: SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
            provider_revision: 9,
            provider_executable: file_binding(
                &binding.executable.path,
                id(&crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 800)),
            ),
            provider_dll: file_binding(
                &binding.zip_dll.path,
                id(&crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 801)),
            ),
            application_image: file_binding(
                &root.path().join("gpui-filer.exe"),
                id(&crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 802)),
            ),
            destination_parent: id(&parent.destination_parent_identity),
            items: descriptors,
        };
        let registration = JournalSevenZipExtractBatch {
            envelope,
            sequence: 1,
            items: Vec::new(),
        };
        assert!(registration.validate().is_ok());
        let mut wrong_attempt = registration.clone();
        wrong_attempt.envelope.attempt.job += 1;
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: wrong_attempt,
        });
        let mut wrong_envelope = registration.clone();
        wrong_envelope.envelope.provider_revision += 1;
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: wrong_envelope,
        });
        let mut wrong_sequence = registration.clone();
        wrong_sequence.sequence = 2;
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: wrong_sequence,
        });
        assert!(
            runtime
                .claim_pending_seven_zip_extract_batch_write(token, &registration)
                .is_none()
        );
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: registration.clone(),
        });
        assert!(
            runtime
                .claim_pending_seven_zip_extract_batch_write(token, &registration)
                .is_some()
        );
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: registration.clone(),
        });
        assert_eq!(
            runtime.seven_zip_extract_batches[&token].pending.as_ref(),
            Some(&registration),
            "duplicate facts cannot overwrite the retained snapshot"
        );
        assert!(
            runtime
                .claim_pending_seven_zip_extract_batch_write(token, &registration)
                .is_none(),
            "duplicate dispatch cannot claim another writer submission"
        );
        let mut wrong_ack = registration.clone();
        wrong_ack.sequence += 1;
        assert!(!runtime.acknowledge_seven_zip_extract_batch(token, &wrong_ack));
        assert!(matches!(
            worker.controls.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(runtime.journal_attempt(token).unwrap())
            .unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &registration));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: registration.clone(),
            })
        );
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: registration.clone(),
        });
        assert!(
            runtime
                .claim_pending_seven_zip_extract_batch_write(token, &registration)
                .is_none()
        );
        assert!(!runtime.acknowledge_seven_zip_extract_batch(token, &registration));
        assert!(matches!(
            worker.controls.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let descriptor = &registration.envelope.items[0];
        let mut ledger: JournalSevenZipExtractLedger =
            super::super::journal::seven_zip_extract_test_fixture();
        ledger.binding.attempt = registration.envelope.attempt.clone();
        ledger.binding.item = 0;
        ledger.binding.source.path = descriptor.source_path.clone();
        ledger.binding.source.identity = descriptor.source_identity.clone();
        ledger.binding.source.length = 7;
        ledger.binding.source.last_write_ticks = 1;
        ledger.binding.provider_revision = registration.envelope.provider_revision;
        ledger.binding.provider_executable = registration.envelope.provider_executable.clone();
        ledger.binding.provider_dll = registration.envelope.provider_dll.clone();
        ledger.binding.application_image = registration.envelope.application_image.clone();
        ledger.binding.destination_parent = registration.envelope.destination_parent.clone();
        ledger.binding.output = descriptor.output.clone();
        ledger.binding.spool = descriptor.spool.clone();
        ledger.binding.tree = descriptor.tree.clone();
        ledger.validate().unwrap();
        let mut invalid_item = ledger.clone();
        invalid_item.binding.item = 2;
        let mut invalid_item_batch = registration.clone();
        invalid_item_batch.sequence = 2;
        invalid_item_batch.items.push(invalid_item);
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: invalid_item_batch,
        });
        assert!(
            runtime
                .claim_pending_seven_zip_extract_batch_write(token, &registration)
                .is_none()
        );

        let mut item_batch = registration.clone();
        item_batch.sequence = 2;
        item_batch.items.push(ledger.clone());
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: item_batch.clone(),
        });
        let item_record = runtime
            .claim_pending_seven_zip_extract_batch_write(token, &item_batch)
            .unwrap();
        store.upsert(item_record).unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &item_batch));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: item_batch.clone()
            })
        );

        runtime.cancel(token);
        assert_eq!(worker.controls.try_recv(), Ok(WorkerControl::Cancel));
        let mut spool_owned = ledger.clone();
        spool_owned.sequence = 2;
        spool_owned.phase = JournalSevenZipExtractPhase::SpoolOwned;
        spool_owned.spool_state = JournalSevenZipExtractObjectState::Owned;
        spool_owned.spool_identity = Some(JournalSevenZipExtractIdentity {
            volume: descriptor.source_identity.volume,
            file_index: 990,
        });
        let mut spool_owned_batch = item_batch.clone();
        spool_owned_batch.sequence = 3;
        spool_owned_batch.items[0] = spool_owned.clone();
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: spool_owned_batch.clone(),
        });
        store
            .upsert(
                runtime
                    .claim_pending_seven_zip_extract_batch_write(token, &spool_owned_batch)
                    .unwrap(),
            )
            .unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &spool_owned_batch));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchObservedExact {
                token,
                batch: spool_owned_batch.clone()
            })
        );
        let mut aborting = spool_owned.clone();
        aborting.sequence = 3;
        aborting.aborting = true;
        let mut abort_batch = spool_owned_batch.clone();
        abort_batch.sequence = 4;
        abort_batch.items[0] = aborting.clone();
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: abort_batch.clone(),
        });
        store
            .upsert(
                runtime
                    .claim_pending_seven_zip_extract_batch_write(token, &abort_batch)
                    .unwrap(),
            )
            .unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &abort_batch));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchObservedExact {
                token,
                batch: abort_batch.clone()
            })
        );
        let mut delete_tree = aborting.clone();
        delete_tree.sequence = 4;
        delete_tree.spool_state = JournalSevenZipExtractObjectState::DeleteIntent;
        let mut cleanup_batch = abort_batch.clone();
        cleanup_batch.sequence = 5;
        cleanup_batch.items[0] = delete_tree;
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: cleanup_batch.clone(),
        });
        store
            .upsert(
                runtime
                    .claim_pending_seven_zip_extract_batch_write(token, &cleanup_batch)
                    .unwrap(),
            )
            .unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &cleanup_batch));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                token,
                batch: cleanup_batch,
                item: ItemId(0),
                target: SevenZipExtractCleanupTarget::Spool,
            })
        );
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Failed,
                final_counters: ProgressCounters {
                    total_items: Some(2),
                    processed_items: 2,
                    ..Default::default()
                },
                receipts: (0..2)
                    .map(|ordinal| ItemReceipt {
                        item: ItemId(ordinal),
                        outcome: ItemOutcome::Failed,
                        detail: Some("fail-closed fixture".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    })
                    .collect(),
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            },
        });
        let terminal = runtime.journal_attempt(token).unwrap();
        assert_eq!(terminal.seven_zip_extract_receipts.len(), 2);
        assert_eq!(
            terminal.state,
            super::super::journal::JournalState::RecoveryRequired,
            "unresolved cleanup custody cannot be released as a clean failure"
        );
        store.upsert(terminal.clone()).unwrap();
        assert!(
            !runtime.acknowledge_terminal_journal(token),
            "generic terminal ACK cannot bypass exact extraction receipt ACK"
        );
        assert!(runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractTerminalJournaledExact {
                token,
                batch: terminal.seven_zip_extract_batch.clone(),
            })
        );
        assert!(
            !runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal),
            "a duplicate durable ACK cannot rearm terminal release"
        );
        let generic = runtime.journal_attempt(token).unwrap();
        assert_eq!(
            generic, terminal,
            "uncertain recovery facts remain retained after terminal ACK"
        );
        store.upsert(generic).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("journal.json")).unwrap())
                .unwrap();
        let saved_attempt = saved["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attempt| attempt["key"] == terminal.key)
            .unwrap();
        assert_eq!(
            saved_attempt["seven_zip_extract_batch"],
            serde_json::to_value(&terminal.seven_zip_extract_batch).unwrap()
        );
        assert_eq!(
            saved_attempt["seven_zip_extract_receipts"],
            serde_json::to_value(&terminal.seven_zip_extract_receipts).unwrap()
        );
        assert_eq!(
            runtime.seven_zip_extract_batches[&token].durable,
            terminal.seven_zip_extract_batch
        );

        let queued = runtime.submit(request).unwrap();
        let _queued_commit = runtime
            .cancel(queued)
            .expect("queued extraction has a no-intent terminal");
        let queued_terminal = runtime.journal_attempt(queued).unwrap();
        assert_eq!(queued_terminal.seven_zip_extract_batch, None);
        assert_eq!(queued_terminal.seven_zip_extract_receipts.len(), 2);
        assert!(
            runtime.abandon_seven_zip_extract_terminal_delivery(queued, &queued_terminal),
            "owner loss closes the reply route before any durable confirmation"
        );
        assert!(!runtime.acknowledge_terminal_journal(queued));
        store.upsert(queued_terminal.clone()).unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_terminal_record(queued, &queued_terminal));
        assert!(
            !runtime.acknowledge_seven_zip_extract_terminal_record(queued, &queued_terminal),
            "persistence retry does not reopen a released terminal route"
        );
        let later_generic = runtime.journal_attempt(queued).unwrap();
        assert!(
            later_generic.seven_zip_extract_receipts.is_empty(),
            "post-ACK projection omits receipts instead of inventing a replacement vector"
        );
        store.upsert(later_generic).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("journal.json")).unwrap())
                .unwrap();
        let queued_saved = saved["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attempt| attempt["key"] == queued_terminal.key)
            .unwrap();
        assert_eq!(
            queued_saved["seven_zip_extract_receipts"],
            serde_json::to_value(&queued_terminal.seven_zip_extract_receipts).unwrap()
        );
    }

    #[test]
    fn failed_abort_replays_before_aborting_late_tree_ownership_without_permission() {
        use super::super::journal::{
            JournalPath, JournalSevenZipExtractAttemptId, JournalSevenZipExtractBatch,
            JournalSevenZipExtractDescriptor, JournalSevenZipExtractEnvelope,
            JournalSevenZipExtractFileBinding, JournalSevenZipExtractIdentity,
            JournalSevenZipExtractLedger, JournalSevenZipExtractObjectState as ObjectState,
            JournalSevenZipExtractPhase as Phase, JournalSevenZipExtractSpoolReady, JournalStore,
            SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
        };
        use super::super::model::{SevenZipExtractGuard, SevenZipExtractSourceGuard};

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("late-tree.7z");
        std::fs::write(&source, b"archive").unwrap();
        let observed =
            crate::operations::model::test_zip_create_guard(&source, &source.with_extension(""));
        let binding = crate::archive_provider::ArchiveOperationBinding {
            executable: crate::archive_provider::ArchiveFileHint {
                path: root.path().join("7z.exe"),
                identity: crate::archive_provider::ArchiveFileIdentity {
                    volume_serial: 1,
                    file_index: 880,
                },
            },
            zip_dll: crate::archive_provider::ArchiveFileHint {
                path: root.path().join("7z.dll"),
                identity: crate::archive_provider::ArchiveFileIdentity {
                    volume_serial: 1,
                    file_index: 881,
                },
            },
        };
        let request = OperationRequest::extract_7z_many(
            vec![source.clone()],
            SevenZipExtractGuard {
                destination_parent_identity: observed.destination_parent_identity.clone(),
                sources: vec![SevenZipExtractSourceGuard {
                    source_identity: observed.source_identity.clone(),
                    source_kind: observed.source_kind,
                    entry_name: observed.entry_name.clone(),
                }],
                provider_revision: 55,
                provider_binding: binding.clone(),
            },
        )
        .unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request.clone()).unwrap();
        let worker = runtime.start_next().unwrap();
        let run_id = runtime.run_id.unwrap();
        let id = |identity: &crate::filesystem_identity::FilesystemIdentity| {
            let (volume, file_index) = identity.windows_parts().unwrap();
            JournalSevenZipExtractIdentity { volume, file_index }
        };
        let file_binding = |path: &std::path::Path, identity| JournalSevenZipExtractFileBinding {
            path: JournalPath::from_path(path),
            identity,
        };
        let envelope = JournalSevenZipExtractEnvelope {
            attempt: JournalSevenZipExtractAttemptId {
                run_id,
                job: token.job_id().value(),
                attempt: token.attempt_id().value(),
            },
            reservation_format: SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
            provider_revision: 55,
            provider_executable: file_binding(
                &binding.executable.path,
                JournalSevenZipExtractIdentity {
                    volume: 1,
                    file_index: 880,
                },
            ),
            provider_dll: file_binding(
                &binding.zip_dll.path,
                JournalSevenZipExtractIdentity {
                    volume: 1,
                    file_index: 881,
                },
            ),
            application_image: file_binding(
                &root.path().join("gpui-filer.exe"),
                JournalSevenZipExtractIdentity {
                    volume: 1,
                    file_index: 882,
                },
            ),
            destination_parent: id(&observed.destination_parent_identity),
            items: vec![JournalSevenZipExtractDescriptor {
                ordinal: 0,
                source_path: JournalPath::from_path(&source),
                source_identity: id(&observed.source_identity),
                output: JournalPath::from_path(&source.with_extension("")),
                spool: JournalPath::from_path(&root.path().join("late.spool")),
                tree: JournalPath::from_path(&root.path().join("late.tree")),
            }],
        };
        let registration = JournalSevenZipExtractBatch {
            envelope: envelope.clone(),
            sequence: 1,
            items: Vec::new(),
        };
        let mut store = JournalStore::open(root.path().join("late-tree-journal.json")).unwrap();
        let persist = |runtime: &mut OperationRuntime,
                       store: &mut JournalStore,
                       batch: &JournalSevenZipExtractBatch| {
            runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
                token,
                batch: batch.clone(),
            });
            let record = runtime
                .claim_pending_seven_zip_extract_batch_write(token, batch)
                .unwrap();
            store.upsert(record).unwrap();
            assert!(runtime.acknowledge_seven_zip_extract_batch(token, batch));
        };
        persist(&mut runtime, &mut store, &registration);
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: registration.clone()
            })
        );

        let descriptor = &envelope.items[0];
        let mut ledger: JournalSevenZipExtractLedger =
            super::super::journal::seven_zip_extract_test_fixture();
        ledger.binding.attempt = envelope.attempt.clone();
        ledger.binding.item = 0;
        ledger.binding.source.path = descriptor.source_path.clone();
        ledger.binding.source.identity = descriptor.source_identity.clone();
        ledger.binding.source.length = 7;
        ledger.binding.source.last_write_ticks = 1;
        ledger.binding.provider_revision = envelope.provider_revision;
        ledger.binding.provider_executable = envelope.provider_executable.clone();
        ledger.binding.provider_dll = envelope.provider_dll.clone();
        ledger.binding.application_image = envelope.application_image.clone();
        ledger.binding.destination_parent = envelope.destination_parent.clone();
        ledger.binding.output = descriptor.output.clone();
        ledger.binding.spool = descriptor.spool.clone();
        ledger.binding.tree = descriptor.tree.clone();
        let mut item_batch = registration.clone();
        item_batch.sequence = 2;
        item_batch.items.push(ledger.clone());
        persist(&mut runtime, &mut store, &item_batch);
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: item_batch.clone()
            })
        );

        ledger.sequence = 2;
        ledger.phase = Phase::SpoolOwned;
        ledger.spool_state = ObjectState::Owned;
        ledger.spool_identity = Some(JournalSevenZipExtractIdentity {
            volume: descriptor.source_identity.volume,
            file_index: 883,
        });
        let mut spool_owned = item_batch.clone();
        spool_owned.sequence = 3;
        spool_owned.items[0] = ledger.clone();
        persist(&mut runtime, &mut store, &spool_owned);
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: spool_owned.clone()
            })
        );
        ledger.sequence = 3;
        ledger.phase = Phase::SpoolReady;
        ledger.spool_ready = Some(JournalSevenZipExtractSpoolReady {
            plan_format: ledger.binding.plan.plan_format,
            plan_sha256: ledger.binding.plan.sha256,
            length: ledger.binding.plan.total_expanded_bytes,
        });
        let mut spool_ready = spool_owned.clone();
        spool_ready.sequence = 4;
        spool_ready.items[0] = ledger.clone();
        persist(&mut runtime, &mut store, &spool_ready);
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: spool_ready.clone()
            })
        );
        ledger.sequence = 4;
        ledger.phase = Phase::TreeCreateIntent;
        ledger.tree_state = ObjectState::CreateIntent;
        let mut tree_intent = spool_ready.clone();
        tree_intent.sequence = 5;
        tree_intent.items[0] = ledger.clone();
        persist(&mut runtime, &mut store, &tree_intent);
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token,
                batch: tree_intent.clone()
            })
        );

        ledger.sequence = 5;
        ledger.aborting = true;
        let mut aborting = tree_intent.clone();
        aborting.sequence = 6;
        aborting.items[0] = ledger.clone();
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: aborting.clone(),
        });
        let failed_record = runtime
            .claim_pending_seven_zip_extract_batch_write(token, &aborting)
            .unwrap();
        assert_eq!(
            failed_record.state,
            super::super::journal::JournalState::Running
        );
        assert!(runtime.reject_seven_zip_extract_batch(token, &aborting));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournalFailedExact {
                token,
                batch: aborting.clone()
            })
        );

        runtime.cancel(token);
        assert_eq!(worker.controls.try_recv(), Ok(WorkerControl::Cancel));
        let cancel_snapshot = runtime.journal_attempt(token).unwrap();
        assert_eq!(
            cancel_snapshot.state,
            super::super::journal::JournalState::CancelRequested
        );
        store.upsert(cancel_snapshot).unwrap();

        ledger.sequence = 6;
        ledger.phase = Phase::TreeOwned;
        ledger.tree_state = ObjectState::Owned;
        ledger.tree_identity = Some(JournalSevenZipExtractIdentity {
            volume: envelope.destination_parent.volume,
            file_index: 884,
        });
        let mut late_owned = aborting.clone();
        late_owned.sequence = 7;
        late_owned.items[0] = ledger;
        runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
            token,
            batch: late_owned.clone(),
        });
        let late_record = runtime
            .claim_pending_seven_zip_extract_batch_write(token, &late_owned)
            .unwrap();
        assert_eq!(
            late_record.seven_zip_extract_batch.as_ref(),
            Some(&late_owned)
        );
        assert_eq!(
            late_record.state,
            super::super::journal::JournalState::CancelRequested
        );
        let (repair_batch, repair_record, pending) =
            runtime.seven_zip_extract_repair_records(token).unwrap();
        assert_eq!(repair_batch, aborting);
        assert_eq!(
            repair_record.state,
            super::super::journal::JournalState::CancelRequested,
            "a retained Running failure is promoted after cancellation was persisted"
        );
        assert!(pending.is_some());
        assert!(
            runtime.reject_seven_zip_extract_repair_batch(token, &aborting),
            "a failed predecessor retry must remain retained with its successor"
        );
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournalFailedExact {
                token,
                batch: aborting.clone(),
            })
        );
        let (retained_predecessor, retained_record, retained_successor) =
            runtime.seven_zip_extract_repair_records(token).unwrap();
        assert_eq!(retained_predecessor, aborting);
        assert_eq!(
            retained_record.state,
            super::super::journal::JournalState::CancelRequested
        );
        assert_eq!(
            retained_successor.as_ref().map(|(batch, _)| batch),
            Some(&late_owned),
            "N and N+1 stay ordered after predecessor retry failure"
        );
        assert!(
            !runtime.acknowledge_seven_zip_extract_repair_batch(token, &late_owned),
            "successor cannot be ACKed before its failed predecessor"
        );
        assert!(matches!(
            worker.controls.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        store.upsert(repair_record).unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_repair_batch(token, &aborting));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchObservedExact {
                token,
                batch: aborting.clone()
            })
        );

        assert!(
            runtime.reject_seven_zip_extract_repair_batch(token, &late_owned),
            "successor failure after predecessor ACK must remain retained for retry"
        );
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractBatchJournalFailedExact {
                token,
                batch: late_owned.clone(),
            })
        );

        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: super::super::model::TerminalReport {
                outcome: super::super::model::TerminalOutcome::Failed,
                final_counters: super::super::model::ProgressCounters::default(),
                receipts: vec![super::super::model::ItemReceipt {
                    item: super::super::model::ItemId(0),
                    outcome: super::super::model::ItemOutcome::Failed,
                    detail: Some("terminal arrived before custody replay".into()),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: super::super::model::RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            },
        });
        let (snapshots, terminal) = runtime
            .seven_zip_extract_terminal_write_plan(token)
            .unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].0, late_owned);
        assert_eq!(
            snapshots[0].1.state,
            super::super::journal::JournalState::CancelRequested,
            "terminal freeze must retain monotone cancellation promotion for pending replay"
        );
        assert_eq!(
            terminal.state,
            super::super::journal::JournalState::RecoveryRequired
        );
        assert!(
            runtime.reject_seven_zip_extract_repair_batch(token, &late_owned),
            "a successor retry failure after terminal freeze must preserve the terminal and custody snapshots"
        );
        assert!(
            runtime
                .frozen_seven_zip_extract_terminal_record(token)
                .is_some()
        );
        let retry_plan = runtime
            .seven_zip_extract_terminal_write_plan(token)
            .unwrap();
        assert_eq!(retry_plan.0.len(), 1);
        assert_eq!(retry_plan.0[0].0, late_owned);
        store.upsert(snapshots[0].1.clone()).unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_repair_batch(token, &late_owned));
        assert!(
            matches!(
                worker.controls.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty
                    | std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "after terminal freeze the worker receives no new permission"
        );
        store.upsert(terminal.clone()).unwrap();
        assert!(runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::SevenZipExtractTerminalJournaledExact {
                token,
                batch: terminal.seven_zip_extract_batch.clone(),
            }),
            "terminal replay ACK releases only the exact terminal barrier"
        );
        assert!(runtime.seven_zip_extract_batches[&token].journal_failed);
        assert_eq!(
            runtime.seven_zip_extract_batches[&token].durable.as_ref(),
            Some(&late_owned)
        );
        assert!(
            matches!(
                worker.controls.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty
                    | std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "late tree ownership grants neither normal nor cleanup permission"
        );
    }

    #[test]
    fn multi_zip_extract_acks_are_attempt_item_and_ledger_bound_and_preserve_prior_items() {
        use super::super::journal::{
            JournalPath, JournalStore, JournalZipExtract, JournalZipExtractPhase,
        };
        let root = tempfile::tempdir().unwrap();
        let sources = [root.path().join("a.zip"), root.path().join("b.zip")];
        for source in &sources {
            std::fs::write(source, b"zip").unwrap();
        }
        let source_guards = sources
            .iter()
            .map(|source| {
                let guard = crate::operations::model::test_zip_create_guard(
                    source,
                    &source.with_extension(""),
                );
                super::super::model::ZipCreateSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }
            })
            .collect();
        let parent = crate::operations::model::test_zip_create_guard(
            &sources[0],
            &root.path().join("unused"),
        );
        let request = OperationRequest::extract_zip_many(
            sources.to_vec(),
            super::super::model::ZipArchiveExtractGuard {
                destination_parent_identity: parent.destination_parent_identity.clone(),
                sources: source_guards,
            },
        )
        .unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request).unwrap();
        let worker = runtime.start_next().unwrap();
        let planned = |index: usize| JournalZipExtract {
            source: JournalPath::from_path(&sources[index]),
            output: JournalPath::from_path(&sources[index].with_extension("")),
            stage: JournalPath::from_path(&root.path().join(format!("stage-{index}"))),
            source_identity: format!(
                "{:?}",
                crate::operations::model::test_zip_create_guard(
                    &sources[index],
                    &sources[index].with_extension("")
                )
                .source_identity
            ),
            parent_identity: format!("{:?}", parent.destination_parent_identity),
            stage_identity: None,
            phase: JournalZipExtractPhase::StagePlanned,
        };
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        let second = planned(1);
        runtime.commit_fact(ExecutorFact::ZipExtractLedgerIntent {
            token,
            item: ItemId(1),
            ledger: second.clone(),
        });
        store
            .upsert(runtime.journal_attempt(token).unwrap())
            .unwrap();
        assert!(!runtime.acknowledge_zip_extract_item_ledger(token, ItemId(0), &second));
        let mut changed = second.clone();
        changed.source_identity.push_str("-changed");
        assert!(!runtime.acknowledge_zip_extract_item_ledger(token, ItemId(1), &changed));
        assert!(runtime.acknowledge_zip_extract_item_ledger(token, ItemId(1), &second));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipExtractLedgerJournaledExact {
                token,
                item: ItemId(1),
                ledger: second.clone()
            })
        );

        let first = planned(0);
        runtime.commit_fact(ExecutorFact::ZipExtractLedgerIntent {
            token,
            item: ItemId(0),
            ledger: first.clone(),
        });
        let record = runtime.journal_attempt(token).unwrap();
        assert_eq!(
            record
                .zip_extract_items
                .iter()
                .map(|entry| entry.item)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        store.upsert(record).unwrap();
        assert!(runtime.acknowledge_zip_extract_item_ledger(token, ItemId(0), &first));
        assert_eq!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipExtractLedgerJournaledExact {
                token,
                item: ItemId(0),
                ledger: first
            })
        );

        for index in 0..2 {
            let mut discarded = planned(index);
            discarded.phase = JournalZipExtractPhase::StageOwned;
            discarded.stage_identity = Some(format!("retained-stage-{index}-id"));
            runtime.commit_fact(ExecutorFact::ZipExtractLedgerIntent {
                token,
                item: ItemId(index as u32),
                ledger: discarded.clone(),
            });
            store
                .upsert(runtime.journal_attempt(token).unwrap())
                .unwrap();
            assert!(runtime.acknowledge_zip_extract_item_ledger(
                token,
                ItemId(index as u32),
                &discarded
            ));
            let _ = worker.controls.try_recv();
            if index == 1 {
                discarded.phase = JournalZipExtractPhase::PublishIntent;
                runtime.commit_fact(ExecutorFact::ZipExtractLedgerIntent {
                    token,
                    item: ItemId(index as u32),
                    ledger: discarded.clone(),
                });
                store
                    .upsert(runtime.journal_attempt(token).unwrap())
                    .unwrap();
                assert!(runtime.acknowledge_zip_extract_item_ledger(
                    token,
                    ItemId(index as u32),
                    &discarded
                ));
                let _ = worker.controls.try_recv();
            }
        }
        let mut terminal = report();
        terminal.outcome = TerminalOutcome::Partial;
        terminal.final_counters.total_items = Some(2);
        terminal.receipts = (0..2)
            .map(|index| super::super::model::ItemReceipt {
                item: ItemId(index),
                outcome: super::super::model::ItemOutcome::Partial,
                detail: None,
                created_folder_path: Some(if index == 0 {
                    root.path().join("stage-0")
                } else {
                    sources[1].with_extension("")
                }),
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            })
            .collect();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: terminal,
            })
            .unwrap();
        let terminal_snapshot = runtime.journal_attempt(token).unwrap();
        assert_eq!(terminal_snapshot.zip_extract_items.len(), 2);
        assert_eq!(terminal_snapshot.zip_extract_receipts.len(), 2);
        store.upsert(terminal_snapshot.clone()).unwrap();
        assert!(store.recoveries().iter().any(|recovery| matches!(
            recovery,
            super::super::journal::RecoveryItem::RecoveryRequired(attempt) if attempt.key == terminal_snapshot.key
        )));
        assert!(runtime.acknowledge_terminal_journal(token));
        assert!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .zip_extract_items
                .is_empty()
        );
    }

    #[test]
    fn unrelated_folder_and_zip_attempts_never_gain_extract_item_facts() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let mut store =
            super::super::journal::JournalStore::open(root.path().join("unrelated.json")).unwrap();
        let folder = OperationRequest {
            kind: super::super::model::OperationKind::CreateFolder,
            virtual_source: None,
            sources: Vec::new(),
            destination: Some(root.path().join("new-folder")),
            move_identity_guard: None,
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
            trash_source_guards: None,
            source_cleanup_identity_guard: None,
            initial_conflict_policy: None,
            copy_destination_identity_guard: None,
            create_parent_directory_identity_guard: Some(
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            ),
            zip_create_guard: None,
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
            batch_rename_plan: None,
        };
        let folder_token = runtime.submit(folder).unwrap();
        runtime.start_next().unwrap();
        let folder_record = runtime.journal_attempt(folder_token).unwrap();
        store.upsert(folder_record.clone()).unwrap();
        assert!(folder_record.zip_extract.is_none());
        assert!(folder_record.zip_extract_items.is_empty());
        assert!(folder_record.zip_extract_receipts.is_empty());
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: folder_token,
                report: report(),
            })
            .unwrap();
        let folder_terminal = runtime.journal_attempt(folder_token).unwrap();
        store.upsert(folder_terminal.clone()).unwrap();
        assert!(folder_terminal.zip_extract_items.is_empty());
        assert!(folder_terminal.zip_extract_receipts.is_empty());

        let source = root.path().join("input.txt");
        std::fs::write(&source, b"zip").unwrap();
        let output = root.path().join("output.zip");
        let zip_token = runtime
            .submit(
                OperationRequest::create_zip(
                    source.clone(),
                    output.clone(),
                    crate::operations::model::test_zip_create_guard(&source, &output),
                )
                .unwrap(),
            )
            .unwrap();
        runtime.start_next().unwrap();
        let zip_record = runtime.journal_attempt(zip_token).unwrap();
        store.upsert(zip_record.clone()).unwrap();
        assert!(zip_record.zip_extract.is_none());
        assert!(zip_record.zip_extract_items.is_empty());
        assert!(zip_record.zip_extract_receipts.is_empty());
    }

    #[test]
    fn cancellation_while_published_zip_receipt_is_pending_rejects_ack_and_reaches_worker() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let second_source = root.path().join("second.txt");
        let output = root.path().join("target.zip");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&second_source, b"second").unwrap();
        let first_guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let second_guard = crate::operations::model::test_zip_create_guard(&second_source, &output);
        let request = OperationRequest::create_zip_many(
            vec![source.clone(), second_source.clone()],
            output.clone(),
            super::super::model::ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity,
                sources: vec![
                    super::super::model::ZipCreateSourceGuard {
                        source_identity: first_guard.source_identity,
                        source_kind: first_guard.source_kind,
                        entry_name: first_guard.entry_name,
                    },
                    super::super::model::ZipCreateSourceGuard {
                        source_identity: second_guard.source_identity,
                        source_kind: second_guard.source_kind,
                        entry_name: second_guard.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        let crate::operations::model::ZipOperationGuard::Create(guard) =
            request.zip_create_guard.as_ref().unwrap().clone()
        else {
            panic!("create guard")
        };
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request).unwrap();
        let worker = runtime.start_next().unwrap();
        let ledger = super::super::journal::JournalZipCreate {
            sources: vec![
                super::super::journal::JournalZipCreateSource {
                    source: super::super::journal::JournalPath::from_path(&source),
                    source_identity: format!("{:?}", guard.sources[0].source_identity),
                },
                super::super::journal::JournalZipCreateSource {
                    source: super::super::journal::JournalPath::from_path(&second_source),
                    source_identity: format!("{:?}", guard.sources[1].source_identity),
                },
            ],
            output: super::super::journal::JournalPath::from_path(&output),
            stage: super::super::journal::JournalPath::from_path(&root.path().join("stage.tmp")),
            parent_identity: format!("{:?}", guard.destination_parent_identity),
            stage_identity: None,
            output_identity: None,
            phase: super::super::journal::JournalZipCreatePhase::StagePlanned,
        };
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
        assert!(matches!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipCreateLedgerJournaledExact { .. })
        ));

        let mut ledger = ledger;
        ledger.stage_identity = Some("owned-stage-id".into());
        ledger.phase = super::super::journal::JournalZipCreatePhase::StageOwned;
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
        assert!(matches!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipCreateLedgerJournaledExact { .. })
        ));

        ledger.phase = super::super::journal::JournalZipCreatePhase::PublishIntent;
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &ledger));
        assert!(matches!(
            worker.controls.try_recv(),
            Ok(WorkerControl::ZipCreateLedgerJournaledExact { .. })
        ));

        ledger.phase = super::super::journal::JournalZipCreatePhase::Published;
        ledger.output_identity = Some("published-output-id".into());
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: ledger.clone(),
        });

        assert!(runtime.cancel(token).is_none());
        assert!(!runtime.acknowledge_zip_create_ledger(token, &ledger));
        assert_eq!(worker.controls.try_recv(), Ok(WorkerControl::Cancel));

        let mut unknown = report();
        unknown.outcome = TerminalOutcome::Unknown;
        unknown.final_counters.total_items = Some(1);
        unknown.retry = super::super::model::RetryPlan::RequiresReinspection;
        unknown.receipts.clear();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: unknown,
            })
            .unwrap();
        let job = runtime.service.job(token.job_id()).unwrap();
        let finished = job
            .attempts
            .iter()
            .find(|attempt| attempt.token == token)
            .unwrap()
            .terminal
            .as_ref()
            .unwrap();
        assert_eq!(finished.outcome, TerminalOutcome::Unknown);
        assert_eq!(job.request.sources.len(), 2);
    }

    #[test]
    fn cancelled_multi_source_zip_accepts_only_its_exact_discard_receipt() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let second = root.path().join("second.txt");
        let output = root.path().join("combined.zip");
        std::fs::write(&first, b"1").unwrap();
        std::fs::write(&second, b"2").unwrap();
        let a = crate::operations::model::test_zip_create_guard(&first, &output);
        let b = crate::operations::model::test_zip_create_guard(&second, &output);
        let request = OperationRequest::create_zip_many(
            vec![first.clone(), second.clone()],
            output.clone(),
            super::super::model::ZipArchiveCreateGuard {
                destination_parent_identity: a.destination_parent_identity.clone(),
                sources: vec![
                    super::super::model::ZipCreateSourceGuard {
                        source_identity: a.source_identity,
                        source_kind: a.source_kind,
                        entry_name: a.entry_name,
                    },
                    super::super::model::ZipCreateSourceGuard {
                        source_identity: b.source_identity,
                        source_kind: b.source_kind,
                        entry_name: b.entry_name,
                    },
                ],
            },
        )
        .unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let token = runtime.submit(request).unwrap();
        let worker = runtime.start_next().unwrap();
        let submitted_job = runtime.service.job(token.job_id()).unwrap();
        let guard = match submitted_job.request.zip_create_guard.as_ref() {
            Some(super::super::model::ZipOperationGuard::Create(guard)) => guard.clone(),
            _ => panic!("create guard"),
        };
        let make_ledger = |phase, stage_identity| super::super::journal::JournalZipCreate {
            sources: [first.clone(), second.clone()]
                .iter()
                .zip(&guard.sources)
                .map(
                    |(path, source)| super::super::journal::JournalZipCreateSource {
                        source: super::super::journal::JournalPath::from_path(path),
                        source_identity: format!("{:?}", source.source_identity),
                    },
                )
                .collect(),
            output: super::super::journal::JournalPath::from_path(&output),
            stage: super::super::journal::JournalPath::from_path(&root.path().join("stage.tmp")),
            parent_identity: format!("{:?}", guard.destination_parent_identity),
            stage_identity,
            output_identity: None,
            phase,
        };
        let planned = make_ledger(
            super::super::journal::JournalZipCreatePhase::StagePlanned,
            None,
        );
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: planned.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &planned));
        assert!(runtime.cancel(token).is_none());
        let discarded = make_ledger(
            super::super::journal::JournalZipCreatePhase::StageDiscarded,
            None,
        );
        runtime.commit_fact(ExecutorFact::ZipCreateLedgerIntent {
            token,
            ledger: discarded.clone(),
        });
        assert!(runtime.acknowledge_zip_create_ledger(token, &discarded));
        let mut forged = discarded;
        forged.sources[1].source_identity = "forged-second-id".into();
        assert!(!runtime.acknowledge_zip_create_ledger(token, &forged));
        assert!(
            matches!(worker.controls.try_recv(), Ok(WorkerControl::ZipCreateLedgerJournaledExact { ledger, .. }) if ledger.phase == super::super::journal::JournalZipCreatePhase::StagePlanned)
        );
        assert!(matches!(
            worker.controls.try_recv(),
            Ok(WorkerControl::Cancel)
        ));
        assert!(
            matches!(worker.controls.try_recv(), Ok(WorkerControl::ZipCreateLedgerJournaledExact { ledger, .. }) if ledger.phase == super::super::journal::JournalZipCreatePhase::StageDiscarded)
        );
    }

    #[test]
    fn replace_backup_intent_is_current_only_for_exact_live_copy_attempt_tuple() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let attempt = runtime.submit(request("source.txt")).unwrap();
        let start = runtime.start_next().unwrap();
        assert_eq!(token(&start), attempt);

        let item = ItemId(0);
        let source = PathBuf::from("source.txt");
        let destination = PathBuf::from("target\\source.txt");
        let backup = PathBuf::from("target\\.gpui-filer-replace-backup");
        runtime.commit_fact(ExecutorFact::ReplaceBackupIntent {
            token: attempt,
            item,
            source: source.clone(),
            destination: destination.clone(),
            backup: backup.clone(),
        });

        let is_current = |runtime: &OperationRuntime,
                          token,
                          item,
                          source: &std::path::Path,
                          destination: &std::path::Path,
                          backup: &std::path::Path| {
            runtime.is_current_replace_backup_intent(token, item, source, destination, backup)
        };
        assert!(is_current(
            &runtime,
            attempt,
            item,
            &source,
            &destination,
            &backup
        ));
        assert!(!is_current(
            &runtime,
            attempt,
            ItemId(1),
            &source,
            &destination,
            &backup
        ));
        assert!(!is_current(
            &runtime,
            attempt,
            item,
            std::path::Path::new("other-source.txt"),
            &destination,
            &backup
        ));
        assert!(!is_current(
            &runtime,
            attempt,
            item,
            &source,
            std::path::Path::new("other-destination.txt"),
            &backup
        ));
        assert!(!is_current(
            &runtime,
            attempt,
            item,
            &source,
            &destination,
            std::path::Path::new("other-backup.txt")
        ));

        // Cancel は即時に intent を無効化する。worker の遅い journal callback は
        // 後から届いても、この attempt の現在性を取り戻せない。
        runtime.cancel(attempt);
        assert!(!is_current(
            &runtime,
            attempt,
            item,
            &source,
            &destination,
            &backup
        ));
    }

    fn pending_move_request(name: &str) -> OperationRequest {
        let mut request = request(name);
        request.kind = OperationKind::Move;
        request.move_identity_guard = Some(MoveIdentityGuard {

            source_identities: Vec::new(),
            destination_identity:
                crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "destination-at-submit".to_owned(),
                ),
            });
        request
    }

    fn running_directory_runtime(items: usize) -> (OperationRuntime, AttemptToken, WorkerStart) {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request(r"C:\source-directory");
        request.move_identity_guard = None;
        request.sources = (0..items)
            .map(|index| PathBuf::from(format!(r"C:\source-{index}")))
            .collect();
        request.destination = Some(PathBuf::from(r"E:\destination"));
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        (runtime, token, start)
    }

    #[test]
    fn missing_worker_terminal_candidate_requires_exact_live_nonterminal_attempt() {
        let (mut runtime, token, _) = running_directory_runtime(1);
        let candidate = runtime
            .missing_worker_terminal_candidate(token)
            .expect("current running attempt gets an Unknown candidate");
        let ExecutorFact::Terminal {
            token: candidate_token,
            report,
        } = candidate
        else {
            panic!("candidate must use the terminal fact route");
        };
        assert_eq!(candidate_token, token);
        assert_eq!(report.outcome, TerminalOutcome::Unknown);
        assert!(report.receipts.is_empty());
        assert!(matches!(
            &report.retry,
            super::super::model::RetryPlan::RequiresReinspection
        ));
        assert_eq!(
            report
                .change_hints
                .iter()
                .map(|hint| hint.directory.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from(r"C:\"), PathBuf::from(r"E:\destination")]
        );

        let stale_token = runtime.submit(request("stale-token")).unwrap();
        assert_ne!(stale_token, token);
        assert!(
            runtime
                .missing_worker_terminal_candidate(stale_token)
                .is_none()
        );

        let _ = runtime.cancel(token);
        let cancelled_candidate = runtime
            .missing_worker_terminal_candidate(token)
            .expect("cancel request is not worker stop confirmation");
        assert!(matches!(
            cancelled_candidate,
            ExecutorFact::Terminal { report, .. }
                if report.outcome == TerminalOutcome::Unknown
        ));
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: report.clone(),
        });
        assert!(runtime.missing_worker_terminal_candidate(token).is_none());

        let (mut retry_runtime, old_token, _) = running_directory_runtime(1);
        retry_runtime.run_id = Some([7; 16]);
        retry_runtime.commit_fact(ExecutorFact::Terminal {
            token: old_token,
            report: TerminalReport {
                outcome: TerminalOutcome::Failed,
                final_counters: ProgressCounters::default(),
                receipts: Vec::new(),
                retry: RetryPlan::Explicit(vec![ItemId(0)]),
                change_hints: Vec::new(),
            },
        });
        let retry_token = retry_runtime.retry(old_token).unwrap();
        retry_runtime.start_next().unwrap();
        assert!(
            retry_runtime
                .missing_worker_terminal_candidate(old_token)
                .is_none()
        );
        assert!(
            retry_runtime
                .missing_worker_terminal_candidate(retry_token)
                .is_some()
        );
    }

    fn keep_both_runtime(
        items: usize,
        apply_to_remaining: bool,
    ) -> (OperationRuntime, AttemptToken, WorkerStart) {
        let (mut runtime, token, start) = running_directory_runtime(items);
        runtime.commit_fact(ExecutorFact::Conflict {
            token,
            notice: super::super::model::ConflictNotice {
                item: super::super::model::ItemId(0),
                probe_revision: 1,
                description: "directory exists".into(),
                replace_supported: false,
                directory_move_replace_rename_candidate: None,
            },
        });
        let conflict = runtime
            .job(token)
            .unwrap()
            .attempts
            .into_iter()
            .find(|attempt| attempt.token == token)
            .unwrap()
            .pending_conflict
            .unwrap()
            .token;
        assert!(
            runtime.resolve_conflict(conflict, ConflictDecision::KeepBoth, apply_to_remaining,)
        );
        (runtime, token, start)
    }

    fn pending_conflict(runtime: &OperationRuntime, token: AttemptToken) -> ConflictToken {
        runtime
            .job(token)
            .unwrap()
            .attempts
            .into_iter()
            .find(|attempt| attempt.token == token)
            .unwrap()
            .pending_conflict
            .unwrap()
            .token
    }

    fn commit_test_conflict(runtime: &mut OperationRuntime, token: AttemptToken) -> ConflictToken {
        runtime.commit_fact(ExecutorFact::Conflict {
            token,
            notice: super::super::model::ConflictNotice {
                item: super::super::model::ItemId(0),
                probe_revision: 1,
                description: "directory exists".into(),
                replace_supported: false,
                directory_move_replace_rename_candidate: None,
            },
        });
        pending_conflict(runtime, token)
    }

    #[test]
    fn missing_conflict_sender_preserves_pending_conflict_and_keep_both_policy() {
        let (mut runtime, token, start) = running_directory_runtime(2);
        let conflict = commit_test_conflict(&mut runtime, token);
        drop(start.controls);
        runtime.controls.remove(&token);

        assert!(!runtime.resolve_conflict(conflict, ConflictDecision::KeepBoth, true));
        let attempt = runtime
            .job(token)
            .unwrap()
            .attempts
            .into_iter()
            .find(|attempt| attempt.token == token)
            .unwrap();
        assert_eq!(
            attempt.state,
            super::super::model::AttemptState::WaitingConflict
        );
        assert_eq!(attempt.pending_conflict.unwrap().token, conflict);
        assert!(
            !runtime
                .keep_both_directory_items
                .iter()
                .any(|(attempt, _)| *attempt == token)
        );
    }

    #[test]
    fn disconnected_conflict_sender_preserves_pending_conflict_and_keep_both_policy() {
        let (mut runtime, token, start) = running_directory_runtime(2);
        let conflict = commit_test_conflict(&mut runtime, token);
        drop(start.controls);

        assert!(!runtime.resolve_conflict(conflict, ConflictDecision::KeepBoth, true));
        let attempt = runtime
            .job(token)
            .unwrap()
            .attempts
            .into_iter()
            .find(|attempt| attempt.token == token)
            .unwrap();
        assert_eq!(
            attempt.state,
            super::super::model::AttemptState::WaitingConflict
        );
        assert_eq!(attempt.pending_conflict.unwrap().token, conflict);
        assert!(
            !runtime
                .keep_both_directory_items
                .iter()
                .any(|(attempt, _)| *attempt == token)
        );
    }

    #[test]
    fn stale_conflict_does_not_enqueue_resolution() {
        let (mut runtime, token, start) = running_directory_runtime(2);
        let stale = commit_test_conflict(&mut runtime, token);
        assert!(runtime.resolve_conflict(stale, ConflictDecision::Skip, false));
        assert!(matches!(
            start.controls.try_recv(),
            Ok(WorkerControl::Resolve { .. })
        ));
        let current = commit_test_conflict(&mut runtime, token);

        assert!(!runtime.resolve_conflict(stale, ConflictDecision::KeepBoth, true));
        assert_eq!(pending_conflict(&runtime, token), current);
        assert!(matches!(
            start.controls.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn current_conflict_enqueues_once_then_resumes_model() {
        let (mut runtime, token, start) = running_directory_runtime(2);
        let conflict = commit_test_conflict(&mut runtime, token);

        assert!(runtime.resolve_conflict(conflict, ConflictDecision::Skip, false));
        assert!(matches!(
            start.controls.try_recv(),
            Ok(WorkerControl::Resolve {
                decision: ConflictDecision::Skip,
                apply_to_remaining: false,
            })
        ));
        assert!(matches!(
            start.controls.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let attempt = runtime
            .job(token)
            .unwrap()
            .attempts
            .into_iter()
            .find(|attempt| attempt.token == token)
            .unwrap();
        assert_eq!(attempt.state, super::super::model::AttemptState::Running);
        assert!(attempt.pending_conflict.is_none());
    }

    fn keep_both_test_ledger(
        item: u32,
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> super::super::journal::JournalDirectoryMove {
        super::super::journal::JournalDirectoryMove {
            item,
            source: super::super::journal::JournalPath::from_path(source),
            destination: super::super::journal::JournalPath::from_path(destination),
            children: Vec::new(),
            pending: None,
            root: Some(super::super::journal::JournalDirectoryRoot {
                source_identity: "source-root-id".into(),
                destination_identity: "destination-root-id".into(),
                pending: None,
                disposition_recorded: false,
            }),
            cleanup_complete: false,
            replace: None,
        }
    }

    fn cleanup_request(path: PathBuf) -> OperationRequest {
        let parent_routes = test_parent_route(&path);
        let leaf_signature = test_leaf_signature();
        OperationRequest {
            kind: OperationKind::ExternalMoveCleanup,
            virtual_source: None,
            sources: vec![path],
            destination: None,
            move_identity_guard: None,
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
            trash_source_guards: None,
            source_cleanup_identity_guard: Some(super::super::model::SourceCleanupIdentityGuard {
                source_identities: vec![
                    crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                        "runtime-cleanup-test".into(),
                    ),
                ],
                parent_routes: vec![parent_routes],
                leaf_signatures: vec![leaf_signature],
            }),
            initial_conflict_policy: None,
            copy_destination_identity_guard: None,
            create_parent_directory_identity_guard: None,
            zip_create_guard: None,
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
            batch_rename_plan: None,
        }
    }

    fn source_cleanup_lease(path: PathBuf) -> SourceCleanupLease {
        let handle = std::fs::File::open(&path).unwrap();
        let routes = vec![test_parent_route(&path)];
        SourceCleanupLease::from_parts(
            vec![path],
            vec![handle],
            routes,
            vec![test_leaf_signature()],
        )
    }

    fn test_leaf_signature() -> super::super::model::SourceCleanupLeafSignature {
        super::super::model::SourceCleanupLeafSignature {
            number_of_links: 1,
            change_time: 7,
        }
    }

    fn test_parent_route(path: &PathBuf) -> Vec<super::super::model::SourceCleanupParentIdentity> {
        let mut parents = path
            .parent()
            .unwrap()
            .ancestors()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        parents.reverse();
        parents
            .into_iter()
            .map(|path| super::super::model::SourceCleanupParentIdentity {
                path,
                identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            })
            .collect()
    }

    fn report() -> TerminalReport {
        TerminalReport {
            outcome: TerminalOutcome::Succeeded,
            final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt {
                item: ItemId(0),
                outcome: ItemOutcome::Completed,
                detail: None,
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: vec![FsChangeHint {
                directory: PathBuf::from("target"),
            }],
        }
    }

    #[test]
    fn zero_change_time_move_proof_never_reaches_runtime_inspection_or_retry_guards() {
        let mut request = request("source.txt");
        request.kind = super::super::model::OperationKind::Move;
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let token = runtime.submit(request).unwrap();
        runtime.start_next().unwrap();
        let proof = super::super::model::CrossVolumeMoveNoEffectProof {
            item: ItemId(0),
            source: PathBuf::from("source.txt"),
            destination: PathBuf::from("target/source.txt"),
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            source_signature: super::super::model::SourceCleanupLeafSignature { number_of_links: 1, change_time: 0 },
            destination_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(3, 4),
        };
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveNoEffectProof { token, proof });
        let mut terminal = report();
        terminal.outcome = TerminalOutcome::Failed;
        terminal.receipts[0].outcome = super::super::model::ItemOutcome::Failed;
        terminal.retry = RetryPlan::RequiresReinspection;
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal }).unwrap();
        runtime.mark_operation_worker_returned(token);
        assert!(runtime.begin_cross_volume_move_inspection(token).is_none());
    }

    fn cross_volume_replace_ledger(
        phase: super::super::journal::CrossVolumeMoveReplacePhase,
    ) -> super::super::journal::JournalCrossVolumeMoveReplace {
        use super::super::journal::{JournalCrossVolumeMoveReplace, JournalPath};
        JournalCrossVolumeMoveReplace {
            item: 0,
            source: JournalPath::from_path(std::path::Path::new(r"C:\source.txt")),
            stage: JournalPath::from_path(std::path::Path::new(r"D:\.stage.txt")),
            destination: JournalPath::from_path(std::path::Path::new(r"D:\target.txt")),
            backup: JournalPath::from_path(std::path::Path::new(r"D:\.backup.txt")),
            source_identity: "00000001:0000000000000001".into(),
            stage_identity: "00000002:0000000000000002".into(),
            approved_target_identity: "00000002:0000000000000003".into(),
            destination_parent_identity: "00000002:0000000000000004".into(),
            content_sha256: "a".repeat(64),
            phase,
        }
    }

    #[test]
    fn stale_cross_volume_replace_phase_ack_cannot_change_terminal_or_retry() {
        use super::super::journal::CrossVolumeMoveReplacePhase as Phase;

        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request(r"C:\source.txt");
        request.destination = Some(PathBuf::from(r"D:\target.txt"));
        let first = runtime.submit(request).unwrap();
        let first_start = runtime.start_next().unwrap();
        assert_eq!(token(&first_start), first);

        let ledger = cross_volume_replace_ledger(Phase::StageReady);
        runtime.commit_fact(ExecutorFact::ReplaceBackupIntent {
            token: first,
            item: ItemId(0),
            source: PathBuf::from(r"C:\source.txt"),
            destination: PathBuf::from(r"D:\target.txt"),
            backup: PathBuf::from(r"D:\.backup.txt"),
        });
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token: first,
            ledger: ledger.clone(),
        });
        assert_eq!(
            runtime.pending_cross_volume_move_replace.get(&first),
            Some(&ledger)
        );
        assert!(runtime.is_current_cross_volume_move_replace_phase(first, &ledger));
        assert!(first_start.controls.try_recv().is_err());

        let mut partial = report();
        partial.outcome = TerminalOutcome::Partial;
        partial.retry = RetryPlan::Explicit(vec![ItemId(0)]);
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: first,
                report: partial,
            })
            .expect("partial attempt terminal commits");

        let terminal_before_late_ack = runtime.journal_attempt(first).unwrap();
        assert!(!runtime.acknowledge_cross_volume_move_replace_phase(first, &ledger));
        assert!(first_start.controls.try_recv().is_err());
        assert_eq!(
            runtime.journal_attempt(first).unwrap(),
            terminal_before_late_ack
        );

        let retry = runtime.retry(first).unwrap();
        let retry_start = runtime.start_next().unwrap();
        assert_eq!(token(&retry_start), retry);
        let retry_ledger = cross_volume_replace_ledger(Phase::StageReady);
        runtime.commit_fact(ExecutorFact::ReplaceBackupIntent {
            token: retry,
            item: ItemId(0),
            source: PathBuf::from(r"C:\source.txt"),
            destination: PathBuf::from(r"D:\target.txt"),
            backup: PathBuf::from(r"D:\.backup.txt"),
        });
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token: retry,
            ledger: retry_ledger.clone(),
        });
        assert_eq!(
            runtime.pending_cross_volume_move_replace.get(&retry),
            Some(&retry_ledger)
        );
        assert!(retry_start.controls.try_recv().is_err());
        let retry_before_late_ack = runtime.journal_attempt(retry).unwrap();

        assert!(!runtime.acknowledge_cross_volume_move_replace_phase(first, &ledger));
        assert!(first_start.controls.try_recv().is_err());
        assert!(retry_start.controls.try_recv().is_err());
        assert_eq!(runtime.pending_cross_volume_move_replace.get(&first), None);
        assert_eq!(
            runtime.pending_cross_volume_move_replace.get(&retry),
            Some(&retry_ledger)
        );
        assert_eq!(
            runtime.journal_attempt(first).unwrap(),
            terminal_before_late_ack
        );
        assert_eq!(
            runtime.journal_attempt(retry).unwrap(),
            retry_before_late_ack
        );
        assert!(retry_before_late_ack.cross_volume_move_replace.is_none());
    }

    #[test]
    fn unaccepted_cross_volume_replace_phase_releases_only_current_waiter() {
        use super::super::journal::CrossVolumeMoveReplacePhase as Phase;

        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request(r"C:\source.txt");
        request.destination = Some(PathBuf::from(r"D:\target.txt"));
        let first = runtime.submit(request).unwrap();
        let first_start = runtime.start_next().unwrap();
        let accepted = cross_volume_replace_ledger(Phase::StageCreateIntent);
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token: first,
            ledger: accepted.clone(),
        });
        let rejected = cross_volume_replace_ledger(Phase::StageOwned);
        assert!(!runtime.is_current_cross_volume_move_replace_phase(first, &rejected));
        assert!(runtime.reject_unaccepted_cross_volume_move_replace_phase(first, &rejected));
        assert!(super::super::executor::wait_for_cross_volume_move_replace_phase_ack(
            first,
            &rejected,
            &first_start.controls,
        )
        .is_err());
        assert!(first_start.cancellation.load(Ordering::Acquire));
        assert_eq!(runtime.pending_cross_volume_move_replace.get(&first), Some(&accepted));

        let mut partial = report();
        partial.outcome = TerminalOutcome::Partial;
        partial.retry = RetryPlan::RequiresReinspection;
        runtime.commit_fact(ExecutorFact::Terminal { token: first, report: partial }).unwrap();
        assert!(!runtime.returned_attempts.contains(&first), "terminal ACK precedes native return");
        runtime.mark_operation_worker_returned(first);
        assert!(runtime.returned_attempts.contains(&first));

        let mut retry_request = pending_move_request(r"C:\source.txt");
        retry_request.destination = Some(PathBuf::from(r"D:\target.txt"));
        let newer = runtime.submit(retry_request).unwrap();
        let newer_start = runtime.start_next().unwrap();
        let newer_ledger = cross_volume_replace_ledger(Phase::StageCreateIntent);
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token: newer,
            ledger: newer_ledger.clone(),
        });
        assert!(!runtime.reject_unaccepted_cross_volume_move_replace_phase(first, &rejected));
        assert!(newer_start.controls.try_recv().is_err());
        assert!(!newer_start.cancellation.load(Ordering::Acquire));
        assert_eq!(runtime.pending_cross_volume_move_replace.get(&newer), Some(&newer_ledger));
    }

    #[test]
    fn stage_create_reservation_commits_only_with_exact_phase_ack() {
        use super::super::journal::{CrossVolumeMoveReplacePhase as Phase, JournalState};

        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let mut request = pending_move_request(r"C:\source.txt");
        request.destination = Some(PathBuf::from(r"D:\target.txt"));
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let mut intent = cross_volume_replace_ledger(Phase::StageCreateIntent);
        intent.stage_identity.clear();
        intent.content_sha256.clear();
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token,
            ledger: intent.clone(),
        });
        let before_ack = runtime.journal_attempt(token).unwrap();
        assert!(before_ack.replace_backup.is_none());
        assert!(before_ack.cross_volume_move_replace.is_none());

        assert!(runtime.acknowledge_cross_volume_move_replace_phase(token, &intent));
        let after_ack = runtime.journal_attempt(token).unwrap();
        assert!(after_ack.replace_backup.is_some());
        assert_eq!(after_ack.cross_volume_move_replace, Some(intent.clone()));
        assert!(matches!(
            start.controls.try_recv(),
            Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournaledExact { .. })
        ));
        assert_eq!(after_ack.state, JournalState::Running);

        let mut failed_runtime = OperationRuntime::new(OperationService::new(2));
        let mut request = pending_move_request(r"C:\source.txt");
        request.destination = Some(PathBuf::from(r"D:\target.txt"));
        let failed_token = failed_runtime.submit(request).unwrap();
        let failed_start = failed_runtime.start_next().unwrap();
        failed_runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token: failed_token,
            ledger: intent.clone(),
        });
        assert!(failed_runtime.reject_cross_volume_move_replace_phase(failed_token, &intent));
        assert!(matches!(
            failed_start.controls.try_recv(),
            Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournalFailedExact { .. })
        ));
        let mut unknown = report();
        unknown.outcome = TerminalOutcome::Unknown;
        failed_runtime
            .commit_fact(ExecutorFact::Terminal {
                token: failed_token,
                report: unknown,
            })
            .unwrap();
        let terminal = failed_runtime.journal_attempt(failed_token).unwrap();
        assert!(terminal.replace_backup.is_none());
        assert!(terminal.cross_volume_move_replace.is_none());
    }

    #[test]
    fn cross_volume_replace_runtime_acknowledges_only_durable_phases_and_persists_success() {
        use super::super::journal::{CrossVolumeMoveReplacePhase as Phase, JournalState};
        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("journal.json");
        let writer = super::super::journal::JournalWriter::start(
            super::super::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request(r"C:\source.txt");
        request.destination = Some(std::path::PathBuf::from(r"D:\target.txt"));
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let ledger = cross_volume_replace_ledger(Phase::StageReady);
        runtime.commit_fact(ExecutorFact::ReplaceBackupIntent {
            token,
            item: ItemId(0),
            source: std::path::PathBuf::from(r"C:\source.txt"),
            destination: std::path::PathBuf::from(r"D:\target.txt"),
            backup: std::path::PathBuf::from(r"D:\.backup.txt"),
        });
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token,
            ledger: ledger.clone(),
        });
        let mut premature = ledger.clone();
        premature.phase = Phase::DisplaceIntent;
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token,
            ledger: premature,
        });
        assert_eq!(
            runtime.pending_cross_volume_move_replace.get(&token),
            Some(&ledger)
        );
        let initial = runtime.journal_attempt(token).unwrap();
        assert!(initial.cross_volume_move_replace.is_none());
        writer
            .upsert_with_reply(initial.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        writer
            .register_cross_volume_move_replace(initial.key.clone(), ledger.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        let mut stale = ledger.clone();
        stale.stage_identity = "00000002:0000000000000099".into();
        assert!(!runtime.acknowledge_cross_volume_move_replace_phase(token, &stale));
        assert!(start.controls.try_recv().is_err());
        assert!(runtime.acknowledge_cross_volume_move_replace_phase(token, &ledger));
        assert!(matches!(
            start.controls.try_recv(),
            Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournaledExact { ledger: ack, .. }) if ack == ledger
        ));
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .cross_volume_move_replace,
            Some(ledger.clone())
        );

        let phases = [
            Phase::DisplaceIntent,
            Phase::TargetDisplaced,
            Phase::PublishIntent,
            Phase::DestinationPublished,
            Phase::SourceDeleteIntent,
            Phase::SourceDeleted,
            Phase::BackupDeleteIntent,
            Phase::BackupDeleted,
            Phase::Complete,
        ];
        let mut current = ledger;
        for phase in phases {
            let mut next = current.clone();
            next.phase = phase;
            runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
                token,
                ledger: next.clone(),
            });
            assert_eq!(
                runtime
                    .journal_attempt(token)
                    .unwrap()
                    .cross_volume_move_replace,
                Some(current.clone())
            );
            writer
                .advance_cross_volume_move_replace(initial.key.clone(), next.clone())
                .recv_blocking()
                .unwrap()
                .unwrap();
            if phase == Phase::Complete {
                // Complete is durable, but Runtime has not yet received its
                // exact owner acknowledgement or the worker terminal fact.
                runtime.cancel(token);
            }
            assert!(runtime.acknowledge_cross_volume_move_replace_phase(token, &next));
            super::super::executor::wait_for_cross_volume_move_replace_phase_ack(
                token,
                &next,
                &start.controls,
            )
            .unwrap();
            current = next;
        }
        let terminal_commit = runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: report(),
            })
            .unwrap();
        assert!(matches!(terminal_commit.event,
            ServiceEvent::TerminalCommitted { report, .. }
                if report.outcome == TerminalOutcome::Succeeded));
        let terminal = runtime.journal_attempt(token).unwrap();
        assert_eq!(terminal.cross_volume_move_replace, Some(current));
        assert!(terminal.replace_backup.is_some());
        assert_eq!(terminal.state, JournalState::Terminal);
        writer
            .upsert_with_reply(terminal)
            .recv_blocking()
            .unwrap()
            .unwrap();
        drop(writer);
        std::thread::yield_now();
        let reopened = super::super::journal::JournalStore::open(journal_path.clone()).unwrap();
        assert!(reopened.recoveries().is_empty());
        let key = reopened.attempt_keys().into_iter().next().unwrap();
        let encoded = std::fs::read_to_string(journal_path).unwrap();
        assert!(encoded.contains(&key));
        assert!(encoded.contains("\"phase\":\"complete\""));
    }

    #[test]
    fn cross_volume_replace_stage_discarded_after_cancel_survives_reopen_without_recovery() {
        use super::super::journal::{CrossVolumeMoveReplacePhase as Phase, JournalState};
        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("journal.json");
        let writer = super::super::journal::JournalWriter::start(
            super::super::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = OperationRuntime::new(OperationService::new(2));
        let mut request = pending_move_request(r"C:\source.txt");
        request.destination = Some(std::path::PathBuf::from(r"D:\target.txt"));
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let mut ledger = cross_volume_replace_ledger(Phase::StageReady);
        runtime.commit_fact(ExecutorFact::ReplaceBackupIntent {
            token,
            item: ItemId(0),
            source: std::path::PathBuf::from(r"C:\source.txt"),
            destination: std::path::PathBuf::from(r"D:\target.txt"),
            backup: std::path::PathBuf::from(r"D:\.backup.txt"),
        });
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token,
            ledger: ledger.clone(),
        });
        let initial = runtime.journal_attempt(token).unwrap();
        writer
            .upsert_with_reply(initial.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        writer
            .register_cross_volume_move_replace(initial.key.clone(), ledger.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();

        // The writer committed StageReady, then cancellation arrived before
        // Runtime received its exact durable receipt.
        runtime.cancel(token);
        writer
            .upsert_with_reply(runtime.journal_attempt(token).unwrap())
            .recv_blocking()
            .unwrap()
            .unwrap();
        assert!(runtime.acknowledge_cross_volume_move_replace_phase(token, &ledger));
        assert!(runtime.cancellations[&token].load(Ordering::Acquire));
        assert_eq!(
            runtime
                .journal_attempt(token)
                .unwrap()
                .cross_volume_move_replace,
            Some(ledger.clone())
        );
        super::super::executor::wait_for_cross_volume_move_replace_phase_ack(
            token,
            &ledger,
            &start.controls,
        )
        .unwrap();

        // Model the worker removing only its held, owned stage before recording
        // the StageDiscarded receipt. Cancellation still blocks disposition.
        let stage_exists = std::cell::Cell::new(true);
        stage_exists.set(false);
        let mut source_disposition_ran = false;
        if !start.cancellation.load(Ordering::Acquire) {
            source_disposition_ran = true;
        }
        assert!(!source_disposition_ran);
        assert!(!stage_exists.get());

        ledger.phase = Phase::StageDiscarded;
        runtime.commit_fact(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token,
            ledger: ledger.clone(),
        });
        writer
            .advance_cross_volume_move_replace(initial.key.clone(), ledger.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        runtime
            .controls
            .get(&token)
            .unwrap()
            .send(WorkerControl::Cancel)
            .unwrap();
        assert!(runtime.acknowledge_cross_volume_move_replace_phase(token, &ledger));
        super::super::executor::wait_for_cross_volume_move_replace_phase_ack(
            token,
            &ledger,
            &start.controls,
        )
        .unwrap();
        let mut cancelled = report();
        cancelled.outcome = TerminalOutcome::Cancelled;
        let _ = runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: cancelled,
        });
        let terminal = runtime.journal_attempt(token).unwrap();
        assert_eq!(terminal.state, JournalState::Terminal);
        assert_eq!(terminal.cross_volume_move_replace, Some(ledger));
        assert!(terminal.replace_backup.is_some());
        writer
            .upsert_with_reply(terminal)
            .recv_blocking()
            .unwrap()
            .unwrap();
        drop(writer);
        std::thread::yield_now();
        let reopened = super::super::journal::JournalStore::open(journal_path).unwrap();
        assert!(reopened.recoveries().is_empty());
    }

    #[test]
    fn directory_move_terminal_is_reserved_durable_and_cancel_safe() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request("source-directory");
        request.move_identity_guard = None;
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        runtime.directory_moves.insert(
            (token, ItemId(0)),
            DirectoryMoveRuntimeState {
                item: ItemId(0),
                source: PathBuf::from("source-directory"),
                destination: PathBuf::from("target").join("source-directory"),
                barrier: DirectoryMoveBarrier::Complete,
            },
        );

        let candidate = runtime
            .directory_move_terminal_candidate(token, &report())
            .unwrap();
        assert_eq!(
            candidate.state,
            super::super::journal::JournalState::Terminal
        );
        assert_eq!(
            candidate.terminal_outcome,
            Some(super::super::journal::JournalTerminalOutcome::Succeeded)
        );
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(0))].barrier,
            DirectoryMoveBarrier::TerminalPending
        );
        // Cleanup is already durable; cancellation after terminal reservation
        // cannot split a durable success record from the Runtime outcome.
        assert!(runtime.cancel(token).is_none());
        assert!(!start.cancellation.load(Ordering::Acquire));
        let commit = runtime
            .commit_directory_move_terminal(token, report(), true)
            .unwrap();
        assert!(
            matches!(commit.event, ServiceEvent::TerminalCommitted { report, .. }
            if report.outcome == TerminalOutcome::Succeeded)
        );
    }

    #[test]
    fn keep_both_authorization_accepts_exact_bounded_candidates_and_commits_terminal() {
        let (mut runtime, token, _start) = keep_both_runtime(2, true);
        let base0 = PathBuf::from(r"E:\destination\source-0");
        let base1 = PathBuf::from(r"E:\destination\source-1");
        let destination2 = super::super::model::keep_both_candidate_path(&base0, 2).unwrap();
        let destination3 = super::super::model::keep_both_candidate_path(&base1, 3).unwrap();
        assert!(super::super::model::is_keep_both_candidate_path(
            &base0,
            &destination2
        ));
        assert!(super::super::model::is_keep_both_candidate_path(
            &base1,
            &destination3
        ));

        for (index, source, destination) in [
            (0, PathBuf::from(r"C:\source-0"), destination2.clone()),
            (1, PathBuf::from(r"C:\source-1"), destination3.clone()),
        ] {
            let ledger = keep_both_test_ledger(index, &source, &destination);
            assert!(
                runtime
                    .commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
                        token,
                        ledger: ledger.clone(),
                    })
                    .is_none()
            );
            assert!(
                runtime
                    .pending_directory_move_manifest(token, &ledger)
                    .is_some()
            );
            assert!(runtime.acknowledge_directory_move_manifest(token, &ledger));
            assert_eq!(
                runtime.directory_moves[&(token, ItemId(index))].destination,
                destination
            );

            let root = ledger.root.as_ref().unwrap();
            let root_intent = super::super::journal::JournalDirectoryRootIntent {
                source_identity: root.source_identity.clone(),
                destination_identity: root.destination_identity.clone(),
            };
            runtime.commit_fact(ExecutorFact::DirectoryMoveRootIntent {
                token,
                item: ItemId(index),
                intent: root_intent.clone(),
            });
            assert!(runtime.acknowledge_directory_move_root_intent(
                token,
                ItemId(index),
                &root_intent,
            ));
            runtime.commit_fact(ExecutorFact::DirectoryMoveRootDisposition {
                token,
                item: ItemId(index),
                intent: root_intent.clone(),
            });
            assert!(runtime.acknowledge_directory_move_root_receipt(
                token,
                ItemId(index),
                &root_intent,
            ));
            runtime.commit_fact(ExecutorFact::DirectoryMoveComplete {
                token,
                item: ItemId(index),
            });
            assert!(runtime.acknowledge_directory_move_complete(token, ItemId(index)));
        }

        let candidate = runtime
            .directory_move_terminal_candidate(token, &report())
            .expect(
                "all authorized candidates must retain their exact destination through completion",
            );
        assert_eq!(
            candidate.terminal_outcome,
            Some(super::super::journal::JournalTerminalOutcome::Succeeded)
        );
        assert!(matches!(
            runtime
                .commit_directory_move_terminal(token, report(), true)
                .unwrap()
                .event,
            ServiceEvent::TerminalCommitted { report, .. }
                if report.outcome == TerminalOutcome::Succeeded
        ));
        assert!(
            !runtime
                .keep_both_directory_items
                .iter()
                .any(|(attempt, _)| *attempt == token)
        );
    }

    #[test]
    fn keep_both_manifest_rejects_missing_auth_wrong_item_arbitrary_candidate_and_cancel() {
        use std::path::Path;

        let base = PathBuf::from(r"E:\destination\source-0");
        let candidate = super::super::model::keep_both_candidate_path(&base, 2).unwrap();
        let arbitrary = PathBuf::from(r"E:\destination\renamed-by-worker");

        let (mut unauthenticated, unauthenticated_token, _start) = running_directory_runtime(1);
        let unauthorized = keep_both_test_ledger(0, Path::new(r"C:\source-0"), &candidate);
        unauthenticated.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token: unauthenticated_token,
            ledger: unauthorized.clone(),
        });
        assert!(
            unauthenticated
                .pending_directory_move_manifest(unauthenticated_token, &unauthorized)
                .is_none()
        );
        assert!(
            !unauthenticated
                .acknowledge_directory_move_manifest(unauthenticated_token, &unauthorized)
        );

        let (mut runtime, token, _start) = keep_both_runtime(2, false);
        let wrong_item = keep_both_test_ledger(1, Path::new(r"C:\source-1"), &candidate);
        runtime.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: wrong_item.clone(),
        });
        assert!(
            runtime
                .pending_directory_move_manifest(token, &wrong_item)
                .is_none()
        );
        assert!(!runtime.acknowledge_directory_move_manifest(token, &wrong_item));

        let arbitrary_ledger = keep_both_test_ledger(0, Path::new(r"C:\source-0"), &arbitrary);
        runtime.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: arbitrary_ledger.clone(),
        });
        assert!(
            runtime
                .pending_directory_move_manifest(token, &arbitrary_ledger)
                .is_none()
        );

        let authorized = keep_both_test_ledger(0, Path::new(r"C:\source-0"), &candidate);
        runtime.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: authorized.clone(),
        });
        assert!(
            runtime
                .pending_directory_move_manifest(token, &authorized)
                .is_some()
        );
        assert!(runtime.acknowledge_directory_move_manifest(token, &authorized));

        runtime.cancel(token);
        runtime.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: authorized.clone(),
        });
        assert!(
            runtime
                .pending_directory_move_manifest(token, &authorized)
                .is_none()
        );
        assert!(!runtime.acknowledge_directory_move_manifest(token, &authorized));
    }

    #[test]
    fn keep_both_authorization_is_attempt_scoped_and_cleared_after_terminal() {
        use std::path::Path;

        let (mut runtime, old_token, _start) = keep_both_runtime(1, false);
        assert!(
            runtime
                .keep_both_directory_items
                .contains(&(old_token, ItemId(0)))
        );
        let mut cancelled = report();
        cancelled.outcome = TerminalOutcome::Cancelled;
        cancelled.retry = RetryPlan::Explicit(vec![ItemId(0)]);
        runtime.commit_fact(ExecutorFact::Terminal {
            token: old_token,
            report: cancelled,
        });
        assert!(
            !runtime
                .keep_both_directory_items
                .iter()
                .any(|(attempt, _)| *attempt == old_token)
        );

        let new_token = runtime.retry(old_token).unwrap();
        runtime.start_next().unwrap();
        let candidate =
            super::super::model::keep_both_candidate_path(Path::new(r"E:\destination\source-0"), 2)
                .unwrap();
        let ledger = keep_both_test_ledger(0, Path::new(r"C:\source-0"), &candidate);
        runtime.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token: new_token,
            ledger: ledger.clone(),
        });
        assert!(
            runtime
                .pending_directory_move_manifest(new_token, &ledger)
                .is_none()
        );
        assert!(!runtime.acknowledge_directory_move_manifest(new_token, &ledger));
    }

    #[test]
    fn directory_move_terminal_requires_every_registered_item_complete() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request("source-directory");
        request.sources.push(PathBuf::from("second-directory"));
        request.move_identity_guard = None;
        let token = runtime.submit(request).unwrap();
        runtime.start_next().unwrap();
        runtime.directory_moves.insert(
            (token, ItemId(0)),
            DirectoryMoveRuntimeState {
                item: ItemId(0),
                source: PathBuf::from("source-directory"),
                destination: PathBuf::from("target").join("source-directory"),
                barrier: DirectoryMoveBarrier::Complete,
            },
        );
        runtime.directory_moves.insert(
            (token, ItemId(1)),
            DirectoryMoveRuntimeState {
                item: ItemId(1),
                source: PathBuf::from("second-directory"),
                destination: PathBuf::from("target").join("second-directory"),
                barrier: DirectoryMoveBarrier::Ready,
            },
        );

        assert!(
            runtime
                .directory_move_terminal_candidate(token, &report())
                .is_none()
        );
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(0))].barrier,
            DirectoryMoveBarrier::Complete
        );
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(1))].barrier,
            DirectoryMoveBarrier::Ready
        );

        runtime
            .directory_moves
            .get_mut(&(token, ItemId(1)))
            .unwrap()
            .barrier = DirectoryMoveBarrier::Complete;
        assert!(
            runtime
                .directory_move_terminal_candidate(token, &report())
                .is_some()
        );
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(0))].barrier,
            DirectoryMoveBarrier::TerminalPending
        );
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(1))].barrier,
            DirectoryMoveBarrier::TerminalPending
        );
    }

    #[test]
    fn directory_move_replace_runtime_terminal_waits_for_exact_complete_ack() {
        use super::super::journal::{
            DirectoryMoveReplacePhase as Phase, JournalDirectoryMoveReplace,
        };

        let (mut runtime, token, _start) = running_directory_runtime(1);
        let source = PathBuf::from(r"C:\source-0");
        let destination = PathBuf::from(r"E:\destination\source-0");
        let replace = JournalDirectoryMoveReplace {
            stage: super::super::journal::JournalPath::from_path(&PathBuf::from(
                r"E:\destination\.stage-source-0",
            )),
            backup: super::super::journal::JournalPath::from_path(&PathBuf::from(
                r"E:\destination\.backup-source-0",
            )),
            stage_identity: "stage-id".into(),
            approved_target_identity: "target-id".into(),
            destination_parent_identity: "parent-id".into(),
            backup_root_identity: "backup-root-id".into(),
            backup_children: Vec::new(),
            backup_pending: None,
            backup_root_disposition_recorded: true,
            backup_cleanup_complete: true,
            phase: Phase::BackupComplete,
        };
        let ledger = super::super::journal::JournalDirectoryMove {
            item: 0,
            source: super::super::journal::JournalPath::from_path(&source),
            destination: super::super::journal::JournalPath::from_path(&destination),
            children: Vec::new(),
            pending: None,
            root: Some(super::super::journal::JournalDirectoryRoot {
                source_identity: "source-root-id".into(),
                destination_identity: "destination-root-id".into(),
                pending: None,
                disposition_recorded: true,
            }),
            cleanup_complete: true,
            replace: Some(replace),
        };
        runtime.commit_fact(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: ledger.clone(),
        });
        assert!(
            runtime
                .pending_directory_move_manifest(token, &ledger)
                .is_some()
        );
        assert!(runtime.acknowledge_directory_move_manifest(token, &ledger));

        runtime.commit_fact(ExecutorFact::DirectoryMoveReplacePhaseIntent {
            token,
            item: ItemId(0),
            phase: Phase::Complete,
        });
        assert!(
            runtime
                .pending_directory_move_replace_phase(token, ItemId(0), Phase::Complete)
                .is_some()
        );
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(0))].barrier,
            DirectoryMoveBarrier::Ready
        );
        assert!(
            runtime
                .directory_move_terminal_candidate(token, &report())
                .is_none()
        );

        assert!(runtime.acknowledge_directory_move_replace_phase(
            token,
            ItemId(0),
            Phase::Complete,
        ));
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(0))].barrier,
            DirectoryMoveBarrier::Complete
        );
        let candidate = runtime
            .directory_move_terminal_candidate(token, &report())
            .expect("the exact Complete ACK permits terminal reservation");
        let completed_replace = &runtime.directory_move_replaces[&(token, ItemId(0))].ledger;
        assert_eq!(completed_replace.phase, Phase::Complete);
        assert!(completed_replace.backup_cleanup_complete);
        assert_eq!(
            runtime.directory_moves[&(token, ItemId(0))].barrier,
            DirectoryMoveBarrier::TerminalPending
        );

        let commit = runtime
            .commit_directory_move_terminal(token, report(), true)
            .expect("reserved terminal record commits");
        assert!(matches!(
            commit.event,
            ServiceEvent::TerminalCommitted { report, .. }
                if report.outcome == TerminalOutcome::Succeeded
        ));
        assert!(
            runtime
                .commit_directory_move_terminal(token, report(), true)
                .is_none()
        );
        assert_eq!(
            candidate.state,
            super::super::journal::JournalState::Terminal
        );
    }

    #[test]
    fn directory_move_replace_rename_cleanup_barriers_are_live_attempt_only() {
        use super::super::journal::{
            JournalDirectoryMoveReplaceRenameChildIntent as CleanupIntent,
            DirectoryMoveReplaceRenamePhase as RenamePhase,
            DirectoryMoveReplaceSourceLocation as SourceLocation,
            DirectoryMoveReplaceTargetLocation as TargetLocation,
            JournalDirectoryMoveReplaceRename as RenameLedger,
            JournalDirectoryNodeKind,
            JournalPath,
        };

        let (mut runtime, token, worker) = running_directory_runtime(1);
        let phase_conflict = commit_test_conflict(&mut runtime, token);
        let stale_phase_ledger = RenameLedger {
            item: 0,
            source: JournalPath { units: Vec::new() },
            destination: JournalPath { units: Vec::new() },
            backup: JournalPath { units: Vec::new() },
            source_identity: "source".into(),
            approved_target_identity: "target".into(),
            volume_identity: "volume".into(),
            source_parent: JournalPath { units: Vec::new() },
            source_parent_identity: "source-parent".into(),
            destination_parent: JournalPath { units: Vec::new() },
            destination_parent_identity: "destination-parent".into(),
            phase: RenamePhase::Prepared,
            source_location: SourceLocation::Original,
            target_location: TargetLocation::Destination,
            approved_target_children: Vec::new(),
            pending_cleanup: None,
            backup_root_disposed: false,
        };
        let intent = CleanupIntent {
            ordinal: 0,
            node_kind: JournalDirectoryNodeKind::File,
            relative_components: vec!["old.txt".encode_utf16().collect::<Vec<u16>>()],
            identity: "old-file-identity".into(),
            content_fact: "old-file-content".into(),
        };
        runtime.pending_directory_move_replace_rename_cleanup.insert(
            token,
            (intent.clone(), RenameCleanupBarrier::IntentPending),
        );
        assert!(runtime.pending_directory_move_replace_rename_cleanup_transition(token, &intent, false));
        let mut mismatched = intent.clone();
        mismatched.content_fact.push_str("-changed");
        assert!(!runtime.pending_directory_move_replace_rename_cleanup_transition(token, &mismatched, false));

        runtime.pending_directory_move_replace_rename_cleanup.get_mut(&token).unwrap().1 =
            RenameCleanupBarrier::BackupCompletePending;
        assert!(runtime.directory_move_replace_rename_backup_complete_pending(token));

        let successor = runtime.submit(pending_move_request(r"C:\queued-source")).unwrap();
        runtime.commit_fact(ExecutorFact::Terminal { token, report: report() })
            .expect("terminal settles the old attempt even when its cleanup barrier is outstanding");
        assert!(!runtime.pending_directory_move_replace_rename_cleanup.contains_key(&token));
        assert!(!runtime.pending_directory_move_replace_rename_cleanup_transition(token, &intent, false));
        assert!(!runtime.pending_directory_move_replace_rename_cleanup_transition(token, &intent, true));
        assert!(!runtime.pending_directory_move_replace_rename_cleanup_transition(token, &mismatched, false));
        assert!(!runtime.directory_move_replace_rename_backup_complete_pending(token));
        assert!(!runtime.acknowledge_directory_move_replace_rename_cleanup_intent(token, &intent));
        assert!(!runtime.acknowledge_directory_move_replace_rename_cleanup_receipt(token, &intent));
        assert!(!runtime.reject_directory_move_replace_rename_cleanup_intent(token, &intent));
        assert!(!runtime.reject_directory_move_replace_rename_cleanup_receipt(token, &intent));
        assert!(!runtime.acknowledge_directory_move_replace_rename_backup_complete(token));
        assert!(!runtime.reject_directory_move_replace_rename_backup_complete(token));
        assert!(worker.controls.try_recv().is_err(), "stale cleanup cannot send a control to the settled worker");

        let successor_worker = runtime.start_next().expect("queued successor starts after terminal");
        assert!(matches!(
            &successor_worker.command,
            super::super::model::ExecutorCommand::Prepare { token, .. } if *token == successor
        ));
        assert_eq!(runtime.running, Some(successor));
        assert!(successor_worker.controls.try_recv().is_err(), "stale cleanup cannot send a control to the successor");
        assert!(!runtime.acknowledge_directory_move_replace_rename_cleanup_intent(token, &intent),
            "late cleanup-intent ACK cannot resume the successor attempt");
        assert!(!runtime.acknowledge_directory_move_replace_rename_cleanup_receipt(token, &intent),
            "late cleanup-receipt ACK cannot resume the successor attempt");
        assert!(!runtime.acknowledge_directory_move_replace_rename_backup_complete(token),
            "late BackupComplete ACK cannot resume the successor attempt");
        assert!(!runtime.acknowledge_directory_move_replace_rename_phase(
            token, phase_conflict, &stale_phase_ledger,
        ), "late phase ACK cannot resume the successor attempt");
        assert!(successor_worker.controls.try_recv().is_err(),
            "late ACKs leave the successor worker control channel untouched");
    }

    #[test]
    fn directory_move_terminal_write_failure_and_pre_reservation_cancel_are_partial() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request("source-directory");
        request.move_identity_guard = None;
        let token = runtime.submit(request).unwrap();
        runtime.start_next().unwrap();
        runtime.directory_moves.insert(
            (token, ItemId(0)),
            DirectoryMoveRuntimeState {
                item: ItemId(0),
                source: PathBuf::from("source-directory"),
                destination: PathBuf::from("target").join("source-directory"),
                barrier: DirectoryMoveBarrier::Complete,
            },
        );
        runtime
            .directory_move_terminal_candidate(token, &report())
            .unwrap();
        let commit = runtime
            .commit_directory_move_terminal(token, report(), false)
            .unwrap();
        assert!(
            matches!(commit.event, ServiceEvent::TerminalCommitted { report, .. }
            if report.outcome == TerminalOutcome::Partial)
        );

        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let mut request = pending_move_request("source-directory");
        request.move_identity_guard = None;
        let token = runtime.submit(request).unwrap();
        runtime.start_next().unwrap();
        runtime.directory_moves.insert(
            (token, ItemId(0)),
            DirectoryMoveRuntimeState {
                item: ItemId(0),
                source: PathBuf::from("source-directory"),
                destination: PathBuf::from("target").join("source-directory"),
                barrier: DirectoryMoveBarrier::Complete,
            },
        );
        assert!(runtime.cancel(token).is_none());
        assert!(
            runtime
                .directory_move_terminal_candidate(token, &report())
                .is_none()
        );
        let commit = runtime
            .commit_directory_move_terminal(token, report(), false)
            .unwrap();
        assert!(
            matches!(commit.event, ServiceEvent::TerminalCommitted { report, .. }
            if report.outcome == TerminalOutcome::Partial)
        );
    }

    fn token(start: &WorkerStart) -> AttemptToken {
        match &start.command {
            ExecutorCommand::Prepare { token, .. } => *token,
            _ => unreachable!(),
        }
    }

    #[test]
    fn one_terminal_commit_releases_the_lane_for_the_next_job() {
        let mut runtime = OperationRuntime::default();
        let first = runtime.submit(request("first")).unwrap();
        runtime.submit(request("second")).unwrap();
        let started = runtime.start_next().unwrap();
        assert_eq!(token(&started), first);
        assert!(runtime.start_next().is_none());
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: first,
                    report: report(),
                })
                .is_some()
        );
        let second = runtime.start_next().unwrap();
        assert_ne!(token(&second), first);
    }

    #[test]
    fn worker_resolved_move_guard_is_committed_before_terminal_and_reused_by_retry() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let first = runtime.submit(pending_move_request("source")).unwrap();
        let started = runtime.start_next().unwrap();
        assert_eq!(token(&started), first);

        let resolved = MoveIdentityGuard {

            source_identities: vec![
                crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "worker-observed-source".to_owned(),
                ),
            ],
            destination_identity:
                crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "destination-at-submit".to_owned(),
                ),
             };
        assert!(
            runtime
                .commit_fact(ExecutorFact::MoveGuardResolved {
                    token: first,
                    guard: resolved.clone(),
                })
                .is_none()
        );

        let mut partial = report();
        partial.outcome = TerminalOutcome::Partial;
        partial.retry = RetryPlan::Explicit(vec![ItemId(0)]);
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: first,
                    report: partial,
                })
                .is_some()
        );

        let stale_guard = MoveIdentityGuard {

            source_identities: vec![
                crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "stale-source".to_owned(),
                ),
            ],
            destination_identity:
                crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "destination-at-submit".to_owned(),
                ),
             };
        assert!(
            runtime
                .commit_fact(ExecutorFact::MoveGuardResolved {
                    token: first,
                    guard: stale_guard.clone(),
                })
                .is_none()
        );

        let retry = runtime.service.retry(first).unwrap();
        let retry_start = runtime.start_next().unwrap();
        assert_eq!(token(&retry_start), retry);
        assert!(
            runtime
                .commit_fact(ExecutorFact::MoveGuardResolved {
                    token: first,
                    guard: stale_guard,
                })
                .is_none()
        );
        let ExecutorCommand::Prepare { request, .. } = retry_start.command else {
            panic!("retry should start as a prepare command")
        };
        assert_eq!(request.move_identity_guard, Some(resolved.clone()));
        assert_eq!(
            runtime.job(first).unwrap().request.move_identity_guard,
            Some(resolved)
        );
    }

    #[test]
    fn held_admission_consumes_capacity_and_is_rejected_when_full() {
        let mut runtime = OperationRuntime::new(OperationService::new(1));
        let held = runtime.submit_held(request("held")).unwrap();

        assert_eq!(
            runtime.submit(request("full")).unwrap_err(),
            SubmitError::CapacityReached
        );
        assert!(runtime.start_next().is_none());
        assert!(runtime.job(held).is_some());
    }

    #[test]
    fn start_next_skips_held_attempt_for_later_eligible_attempt() {
        let mut runtime = OperationRuntime::default();
        let held = runtime.submit_held(request("held")).unwrap();
        let eligible = runtime.submit(request("eligible")).unwrap();

        let started = runtime.start_next().unwrap();
        assert_eq!(token(&started), eligible);
        assert!(!runtime.release_held(eligible));
        assert!(runtime.job(held).is_some());
    }

    #[test]
    fn exact_release_makes_held_attempt_runnable_once() {
        let mut runtime = OperationRuntime::default();
        let held = runtime.submit_held(request("held")).unwrap();
        let unknown = runtime.submit(request("other")).unwrap();

        assert!(!runtime.release_held(unknown));
        assert!(runtime.release_held(held));
        assert!(!runtime.release_held(held));
        assert_eq!(token(&runtime.start_next().unwrap()), held);
    }

    #[test]
    fn cancelling_held_attempt_prevents_later_release() {
        let mut runtime = OperationRuntime::default();
        let held = runtime.submit_held(request("held")).unwrap();

        assert!(runtime.cancel(held).is_some());
        assert!(!runtime.release_held(held));
        assert!(runtime.start_next().is_none());
        assert!(runtime.submit(request("capacity released")).is_ok());
    }

    #[test]
    fn retry_rejects_when_held_attempt_uses_the_available_capacity() {
        let mut runtime = OperationRuntime::new(OperationService::new(1));
        let first = runtime.submit(request("retryable")).unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: first,
                report: TerminalReport {
                    retry: RetryPlan::Explicit(vec![ItemId(0)]),
                    ..report()
                },
            })
            .unwrap();
        let held = runtime.submit_held(request("held")).unwrap();
        let prior_history = runtime.job(first).unwrap().attempts.len();

        assert_eq!(
            runtime.service.retry(first),
            Err(RetryError::CapacityReached)
        );
        assert_eq!(runtime.job(first).unwrap().attempts.len(), prior_history);
        assert_eq!(runtime.job(held).unwrap().attempts.len(), 1);
        assert!(runtime.start_next().is_none());
        assert_eq!(
            runtime.submit(request("still full")).unwrap_err(),
            SubmitError::CapacityReached
        );
    }

    #[test]
    fn held_attempt_rejects_progress_until_executor_claims_it() {
        let mut runtime = OperationRuntime::default();
        let held = runtime.submit_held(request("held")).unwrap();
        let update = ProgressUpdate {
            sequence: 1,
            counters: ProgressCounters {
                processed_bytes: 5,
                ..Default::default()
            },
            current_item: Some(ItemId(0)),
        };

        assert!(!runtime.commit_progress(held, &update));
        assert!(runtime.release_held(held));
        assert!(!runtime.commit_progress(held, &update));
        assert_eq!(token(&runtime.start_next().unwrap()), held);
        assert!(runtime.commit_progress(held, &update));
    }

    #[test]
    fn start_next_transfers_cleanup_lease_once_by_attempt_token() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"source").unwrap();
        let mut runtime = OperationRuntime::default();
        let token = runtime
            .submit_external_move_cleanup(
                cleanup_request(path.clone()),
                source_cleanup_lease(path.clone()),
            )
            .unwrap();

        let started = runtime.start_next().unwrap();
        assert_eq!(self::token(&started), token);
        let lease = started
            .source_cleanup_lease
            .expect("lease transfers to worker");
        assert_eq!(lease.paths, vec![path]);
        assert_eq!(lease.handles.len(), 1);
        assert!(!runtime.source_cleanup_leases.contains_key(&token));
    }

    #[test]
    fn rejected_cleanup_submission_drops_lease() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"source").unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(1));
        runtime.submit(request("occupies capacity")).unwrap();

        let error = runtime
            .submit_external_move_cleanup(cleanup_request(path.clone()), source_cleanup_lease(path))
            .unwrap_err();

        assert_eq!(
            error,
            SourceCleanupSubmitError::Rejected(SubmitError::CapacityReached)
        );
        assert!(runtime.source_cleanup_leases.is_empty());
    }

    #[test]
    fn cleanup_submission_rejects_a_leaf_signature_not_paired_with_its_lease() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"source").unwrap();
        let mut request = cleanup_request(path.clone());
        request
            .source_cleanup_identity_guard
            .as_mut()
            .unwrap()
            .leaf_signatures[0]
            .change_time += 1;
        let lease = source_cleanup_lease(path);
        let mut runtime = OperationRuntime::default();

        assert_eq!(
            runtime
                .submit_external_move_cleanup(request, lease)
                .unwrap_err(),
            SourceCleanupSubmitError::InvalidLease
        );
        assert!(runtime.source_cleanup_leases.is_empty());
    }

    #[test]
    fn queued_cleanup_cancellation_releases_lease() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"source").unwrap();
        let mut runtime = OperationRuntime::default();
        let token = runtime
            .submit_external_move_cleanup(cleanup_request(path.clone()), source_cleanup_lease(path))
            .unwrap();
        assert!(runtime.source_cleanup_leases.contains_key(&token));

        assert!(runtime.cancel(token).is_some());

        assert!(!runtime.source_cleanup_leases.contains_key(&token));
    }

    #[test]
    fn cancel_sets_only_the_current_attempts_flag_until_terminal() {
        let mut runtime = OperationRuntime::default();
        let requested = runtime.submit(request("first")).unwrap();
        let started = runtime.start_next().unwrap();
        assert_eq!(token(&started), requested);
        assert!(!started.cancellation.load(Ordering::Acquire));
        assert!(runtime.cancel(requested).is_none());
        assert!(started.cancellation.load(Ordering::Acquire));
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: requested,
                    report: report(),
                })
                .is_some()
        );
        assert!(runtime.start_next().is_none());
    }

    #[test]
    fn stale_terminal_cannot_release_a_new_attempts_cancellation() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let first = runtime.submit(request("first")).unwrap();
        let first_start = runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: first,
                report: TerminalReport {
                    retry: RetryPlan::Explicit(vec![ItemId(0)]),
                    ..report()
                },
            })
            .unwrap();
        let retry = runtime.service.retry(first).unwrap();
        let retry_start = runtime.start_next().unwrap();
        assert_eq!(token(&retry_start), retry);
        assert!(runtime.cancel(retry).is_none());
        assert!(retry_start.cancellation.load(Ordering::Acquire));
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: first,
                    report: report(),
                })
                .is_none()
        );
        assert!(retry_start.cancellation.load(Ordering::Acquire));
        assert!(!first_start.cancellation.load(Ordering::Acquire));
    }

    #[test]
    fn late_progress_and_terminal_cannot_overwrite_a_retried_attempt() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let first = runtime.submit(request("first")).unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: first,
                report: TerminalReport {
                    retry: RetryPlan::Explicit(vec![ItemId(0)]),
                    ..report()
                },
            })
            .unwrap();
        let retry = runtime.service.retry(first).unwrap();
        runtime.start_next().unwrap();
        assert!(runtime.commit_progress(
            retry,
            &ProgressUpdate {
                sequence: 1,
                counters: ProgressCounters {
                    processed_bytes: 5,
                    ..Default::default()
                },
                current_item: Some(ItemId(0)),
            },
        ));
        assert!(!runtime.commit_progress(
            first,
            &ProgressUpdate {
                sequence: 99,
                counters: ProgressCounters {
                    processed_bytes: 999,
                    ..Default::default()
                },
                current_item: Some(ItemId(0)),
            },
        ));
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: first,
                    report: report(),
                })
                .is_none()
        );

        let job = runtime.job(retry).unwrap();
        let attempt = job.attempts.last().unwrap();
        assert_eq!(attempt.token, retry);
        assert_eq!(
            attempt.progress.as_ref().unwrap().counters.processed_bytes,
            5
        );
    }

    #[test]
    fn batch_rename_undo_retry_keeps_original_order_and_ignores_late_attempt_facts() {
        let path = |name: &str| PathBuf::from(format!(r"C:\batch\{name}"));
        let target = |name: &str| super::super::batch_rename::RenameTarget {
            source: path(name),
            destination: path(&format!("{name}-renamed")),
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                format!("source-{name}"),
            ),
            parent_identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                "parent".to_owned(),
            ),
            kind: crate::directory::EntryKind::File,
            reparse_guard: None,
        };
        let originals = vec![target("A"), target("B"), target("C")];
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let rename = runtime
            .submit_batch_rename(originals.clone(), None)
            .unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: rename,
                report: TerminalReport {
                    receipts: (0..3)
                        .map(|item| ItemReceipt {
                            item: ItemId(item),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        })
                        .collect(),
                    ..report()
                },
            })
            .expect("original batch rename commits");

        let (
            reservation,
            UndoEntry::BatchRename {
                receipt_id,
                targets: reserved,
            },
        ) = runtime.reserve_undo().unwrap()
        else {
            panic!("batch rename receipt expected");
        };
        assert_eq!(reserved, originals);
        assert!(runtime.finish_trash_undo(reservation, true));
        let first = runtime
            .submit_batch_rename_undo(receipt_id, reserved, None)
            .unwrap();
        let first_start = runtime.start_next().unwrap();
        let ExecutorCommand::Prepare {
            token: started_token,
            request: first_request,
            ..
        } = &first_start.command
        else {
            panic!("prepare command expected");
        };
        assert_eq!(*started_token, first);
        let first_plan = first_request.batch_rename_plan.as_ref().unwrap();
        assert_eq!(
            first_plan
                .iter()
                .map(|item| &item.source)
                .collect::<Vec<_>>(),
            vec![&path("C-renamed"), &path("B-renamed"), &path("A-renamed")]
        );

        runtime.commit_fact(ExecutorFact::BatchRenameItemCompleted {
            token: first,
            item: ItemId(0),
            source: path("C-renamed"),
            destination: path("C"),
            source_identity: "first-source".to_owned(),
            parent_identity: "first-parent".to_owned(),
        });
        assert_eq!(
            runtime.pending_batch_rename_items.get(&first),
            Some(&(ItemId(0), path("C-renamed"), path("C")))
        );

        let reverse_report = TerminalReport {
            outcome: TerminalOutcome::Partial,
            receipts: vec![
                ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::Completed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                },
                ItemReceipt {
                    item: ItemId(1),
                    outcome: ItemOutcome::Failed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                },
                ItemReceipt {
                    item: ItemId(2),
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                },
            ],
            retry: RetryPlan::Explicit(vec![ItemId(1), ItemId(2)]),
            ..report()
        };
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: first,
                report: reverse_report,
            })
            .expect("first undo terminal commits");
        let expected_retained = vec![target("A"), target("B")];
        let [
            UndoEntry::BatchRename {
                receipt_id: retained_id,
                targets: retained,
            },
        ] = runtime.undo_ledger.as_slice()
        else {
            panic!("one retained batch undo receipt expected");
        };
        assert_eq!(*retained_id, receipt_id);
        assert_eq!(retained, &expected_retained);

        let retry = runtime.retry(first).unwrap();
        let retry_start = runtime.start_next().unwrap();
        let ExecutorCommand::Prepare {
            token: retry_token,
            request: retry_request,
            retry_items,
            ..
        } = &retry_start.command
        else {
            panic!("prepare command expected");
        };
        assert_eq!(*retry_token, retry);
        assert_eq!(retry_items, &vec![ItemId(1), ItemId(2)]);
        let retry_plan = retry_request.batch_rename_plan.as_ref().unwrap();
        assert_eq!(
            retry_items
                .iter()
                .map(|item| &retry_plan[item.0 as usize].source)
                .collect::<Vec<_>>(),
            vec![&path("B-renamed"), &path("A-renamed")]
        );

        let first_completed_before_late_facts = runtime.batch_rename_completed[&first].clone();
        let first_pending_before_late_facts = runtime.pending_batch_rename_items[&first].clone();

        let late_progress = ProgressUpdate {
            sequence: 99,
            counters: ProgressCounters {
                processed_items: 3,
                ..ProgressCounters::default()
            },
            current_item: Some(ItemId(0)),
        };
        assert!(!runtime.commit_progress(first, &late_progress));
        assert!(
            runtime
                .commit_fact(ExecutorFact::BatchRenameItemCompleted {
                    token: first,
                    item: ItemId(0),
                    source: path("C-renamed"),
                    destination: path("C"),
                    source_identity: "late-source".to_owned(),
                    parent_identity: "late-parent".to_owned(),
                })
                .is_none()
        );
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: first,
                    report: report(),
                })
                .is_none()
        );
        assert_eq!(
            runtime.batch_rename_completed[&first],
            first_completed_before_late_facts
        );
        assert_eq!(
            runtime.pending_batch_rename_items[&first],
            first_pending_before_late_facts
        );

        let [
            UndoEntry::BatchRename {
                receipt_id: retained_id,
                targets: retained,
            },
        ] = runtime.undo_ledger.as_slice()
        else {
            panic!("late facts must leave one retained batch undo receipt");
        };
        assert_eq!(*retained_id, receipt_id);
        assert_eq!(retained, &expected_retained);
        let job = runtime.job(retry).unwrap();
        assert_eq!(job.attempts.len(), 2);
        assert_eq!(job.attempts[1].token, retry);
        assert!(job.attempts[1].progress.is_none());
        assert!(job.attempts[1].terminal.is_none());

        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: retry,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    receipts: vec![
                        ItemReceipt {
                            item: ItemId(1),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                        ItemReceipt {
                            item: ItemId(2),
                            outcome: ItemOutcome::Failed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                    ],
                    retry: RetryPlan::Explicit(vec![ItemId(2)]),
                    ..report()
                },
            })
            .expect("retry partial terminal commits");
        let [
            UndoEntry::BatchRename {
                receipt_id: retained_id,
                targets: retained,
            },
        ] = runtime.undo_ledger.as_slice()
        else {
            panic!("retry partial must retain only unresolved A");
        };
        assert_eq!(*retained_id, receipt_id);
        assert_eq!(retained, &vec![target("A")]);

        let final_retry = runtime.retry(retry).unwrap();
        let final_start = runtime.start_next().unwrap();
        let ExecutorCommand::Prepare {
            token: final_token,
            request: final_request,
            retry_items: final_items,
            ..
        } = &final_start.command
        else {
            panic!("prepare command expected");
        };
        assert_eq!(*final_token, final_retry);
        assert_eq!(final_items, &vec![ItemId(2)]);
        let final_plan = final_request.batch_rename_plan.as_ref().unwrap();
        assert_eq!(final_plan[2].source, path("A-renamed"));
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: final_retry,
                report: TerminalReport {
                    outcome: TerminalOutcome::Succeeded,
                    receipts: vec![ItemReceipt {
                        item: ItemId(2),
                        outcome: ItemOutcome::Completed,
                        detail: None,
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    }],
                    retry: RetryPlan::NotAllowed,
                    ..report()
                },
            })
            .expect("final retry terminal commits");
        assert!(runtime.undo_ledger.is_empty());
        assert!(!runtime.batch_undo_receipts.contains_key(&retry.job_id()));
    }

    #[test]
    fn stale_batch_undo_retry_cannot_consume_an_identical_newer_receipt() {
        let path = |name: &str| PathBuf::from(format!(r"C:\batch\{name}"));
        let target = |name: &str| super::super::batch_rename::RenameTarget {
            source: path(name),
            destination: path(&format!("{name}-renamed")),
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                format!("source-{name}"),
            ),
            parent_identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                "parent".to_owned(),
            ),
            kind: crate::directory::EntryKind::File,
            reparse_guard: None,
        };
        let mut runtime = OperationRuntime::new(OperationService::new(8));
        let old_targets = vec![target("A"), target("B")];
        let old_rename = runtime
            .submit_batch_rename(old_targets.clone(), None)
            .unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: old_rename,
                report: TerminalReport {
                    receipts: (0..2)
                        .map(|item| ItemReceipt {
                            item: ItemId(item),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        })
                        .collect(),
                    ..report()
                },
            })
            .unwrap();
        let (
            reservation,
            UndoEntry::BatchRename {
                receipt_id: old_id,
                targets,
            },
        ) = runtime.reserve_undo().unwrap()
        else {
            panic!("old batch receipt expected");
        };
        assert!(runtime.finish_trash_undo(reservation, true));
        let old_undo = runtime
            .submit_batch_rename_undo(old_id, targets, None)
            .unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: old_undo,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    receipts: vec![
                        ItemReceipt {
                            item: ItemId(0),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                        ItemReceipt {
                            item: ItemId(1),
                            outcome: ItemOutcome::Failed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                    ],
                    retry: RetryPlan::Explicit(vec![ItemId(1)]),
                    ..report()
                },
            })
            .unwrap();

        let newer_targets = vec![target("A")];
        let newer_job = runtime
            .submit_batch_rename(newer_targets.clone(), None)
            .unwrap();
        let newer_id = BatchRenameUndoId(newer_job.job_id());
        runtime.undo_ledger.clear();
        runtime.undo_ledger.push(UndoEntry::BatchRename {
            receipt_id: newer_id,
            targets: newer_targets.clone(),
        });

        assert_eq!(runtime.retry(old_undo), Err(RetryError::NotRetryable));
        let [
            UndoEntry::BatchRename {
                receipt_id,
                targets,
            },
        ] = runtime.undo_ledger.as_slice()
        else {
            panic!("newer duplicate receipt must remain the sole ledger entry");
        };
        assert_eq!(*receipt_id, newer_id);
        assert_eq!(targets, &newer_targets);
        assert_ne!(old_id, newer_id);
    }

    #[test]
    fn unknown_batch_undo_item_is_excluded_but_untouched_suffix_retries() {
        let path = |name: &str| PathBuf::from(format!(r"C:\batch\{name}"));
        let target = |name: &str| super::super::batch_rename::RenameTarget {
            source: path(name),
            destination: path(&format!("{name}-renamed")),
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                format!("source-{name}"),
            ),
            parent_identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                "parent".to_owned(),
            ),
            kind: crate::directory::EntryKind::File,
            reparse_guard: None,
        };
        let original = vec![target("A"), target("B"), target("C"), target("D")];
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let rename = runtime.submit_batch_rename(original.clone(), None).unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: rename,
                report: TerminalReport {
                    receipts: (0..4)
                        .map(|item| ItemReceipt {
                            item: ItemId(item),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        })
                        .collect(),
                    ..report()
                },
            })
            .unwrap();
        let (
            reservation,
            UndoEntry::BatchRename {
                receipt_id,
                targets,
            },
        ) = runtime.reserve_undo().unwrap()
        else {
            panic!("batch receipt expected");
        };
        assert!(runtime.finish_trash_undo(reservation, true));
        let undo = runtime
            .submit_batch_rename_undo(receipt_id, targets, None)
            .unwrap();
        runtime.start_next().unwrap();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: undo,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    receipts: vec![
                        ItemReceipt {
                            item: ItemId(0),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                        ItemReceipt {
                            item: ItemId(1),
                            outcome: ItemOutcome::Unknown,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                        ItemReceipt {
                            item: ItemId(2),
                            outcome: ItemOutcome::CancelledBeforeStart,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                        ItemReceipt {
                            item: ItemId(3),
                            outcome: ItemOutcome::CancelledBeforeStart,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                    ],
                    retry: RetryPlan::RequiresReinspection,
                    ..report()
                },
            })
            .unwrap();

        let known_untouched = vec![target("A"), target("B")];
        let [
            UndoEntry::BatchRename {
                receipt_id: retained_id,
                targets: retained,
            },
        ] = runtime.undo_ledger.as_slice()
        else {
            panic!("known untouched suffix should remain undoable");
        };
        assert_eq!(*retained_id, receipt_id);
        assert_eq!(retained, &known_untouched);

        assert_eq!(runtime.retry(undo), Err(RetryError::NotRetryable));
        let (
            reservation,
            UndoEntry::BatchRename {
                receipt_id: retry_receipt_id,
                targets,
            },
        ) = runtime.reserve_undo().unwrap()
        else {
            panic!("known untouched suffix should be reservable again");
        };
        assert_eq!(retry_receipt_id, receipt_id);
        assert_eq!(targets, known_untouched);
        assert!(runtime.finish_trash_undo(reservation, true));
        let fresh_undo = runtime
            .submit_batch_rename_undo(retry_receipt_id, targets, None)
            .unwrap();
        let start = runtime.start_next().unwrap();
        let ExecutorCommand::Prepare {
            token,
            request,
            retry_items: _,
            ..
        } = &start.command
        else {
            panic!("prepare command expected");
        };
        assert_eq!(*token, fresh_undo);
        let plan = request.batch_rename_plan.as_ref().unwrap();
        assert_eq!(
            plan.iter().map(|item| &item.source).collect::<Vec<_>>(),
            vec![&path("B-renamed"), &path("A-renamed")]
        );
    }

    #[test]
    fn partial_replace_backup_stays_recovery_required_in_the_journal_projection() {
        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(request("source.txt")).unwrap();
        let _start = runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::ReplaceBackupIntent {
            token,
            item: ItemId(0),
            source: PathBuf::from("source.txt"),
            destination: PathBuf::from("target\\source.txt"),
            backup: PathBuf::from("target\\.gpui-filer-replace-backup"),
        });
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    final_counters: ProgressCounters::default(),
                    receipts: Vec::new(),
                    retry: RetryPlan::RequiresReinspection,
                    change_hints: Vec::new(),
                },
            })
            .unwrap();
        let record = runtime.journal_attempt(token).unwrap();
        assert_eq!(
            record.state,
            super::super::journal::JournalState::RecoveryRequired
        );
        assert!(record.replace_backup.is_some());
    }

    #[test]
    fn guarded_move_no_effect_proof_is_passive_exact_attempt_evidence() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let source = PathBuf::from("source.txt");
        let destination = PathBuf::from("target/source.txt");
        let attempt = runtime.submit(pending_move_request("source.txt")).unwrap();
        let started = runtime.start_next().unwrap();
        assert_eq!(token(&started), attempt);
        let source_identity = crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
            "accepted-source".to_owned(),
        );
        let parent_identity = crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
            "destination-at-submit".to_owned(),
        );
        assert!(
            runtime
                .commit_fact(ExecutorFact::MoveGuardResolved {
                    token: attempt,
                    guard: MoveIdentityGuard {

                        source_identities: vec![source_identity.clone()],
                        destination_identity: parent_identity.clone(),
                         },
                })
                .is_none()
        );
        let proof = super::super::model::GuardedMoveNoEffectProof {
            item: ItemId(0),
            source: source.clone(),
            destination: destination.clone(),
            source_identity,
            destination_parent_identity: parent_identity,
        };
        let fact = ExecutorFact::GuardedMoveNoEffectProof {
            token: attempt,
            proof: proof.clone(),
        };
        assert!(runtime.guarded_move_no_effect_proofs(attempt).is_none());
        assert!(runtime.commit_fact(fact.clone()).is_none());
        assert!(runtime.commit_fact(fact.clone()).is_none());
        assert!(runtime.guarded_move_no_effect_proofs(attempt).is_none());

        let mut terminal = report();
        terminal.outcome = TerminalOutcome::Failed;
        terminal.retry = RetryPlan::RequiresReinspection;
        terminal.receipts[0].outcome = super::super::model::ItemOutcome::Failed;
        terminal.receipts[0].move_receipt = Some(super::super::model::MoveReceipt {
            source: source.clone(),
            destination: destination.clone(),
            destination_published: false,
            source_removed: false,
        });
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token: attempt,
                    report: terminal
                })
                .is_some()
        );
        assert_eq!(
            runtime.guarded_move_no_effect_proofs(attempt),
            Some(vec![proof])
        );
        assert!(
            runtime.commit_fact(fact).is_none(),
            "late duplicate proof must be ignored"
        );
        assert_eq!(
            runtime
                .guarded_move_no_effect_proofs(attempt)
                .unwrap()
                .len(),
            1
        );
        assert!(
            runtime.begin_guarded_move_inspection(attempt).is_none(),
            "terminal fact is not worker return"
        );
        runtime.mark_operation_worker_returned(attempt);
        let first_inspection = runtime.begin_guarded_move_inspection(attempt).unwrap();
        assert_eq!(first_inspection.ticket.value(), 1);
        let second_inspection = runtime.begin_guarded_move_inspection(attempt).unwrap();
        assert_eq!(
            second_inspection.ticket.value(),
            2,
            "tickets are monotonic and replacement supersedes"
        );
        assert!(
            !runtime.finish_guarded_move_inspection(first_inspection),
            "superseded result is stale"
        );
        assert!(runtime.finish_guarded_move_inspection(second_inspection.clone()));
        assert!(
            !runtime.finish_guarded_move_inspection(second_inspection.clone()),
            "result commits once"
        );
        assert_eq!(
            runtime.guarded_move_inspection(second_inspection.ticket),
            Some(&second_inspection)
        );
        assert_eq!(runtime.retry(attempt), Err(RetryError::NotRetryable));
        assert!(runtime.guarded_move_no_effect_proofs(attempt).is_some());
        let cancelled_inspection = runtime.begin_guarded_move_inspection(attempt).unwrap();
        assert!(runtime.cancel_guarded_move_inspection(cancelled_inspection.ticket));
        assert!(
            !runtime.finish_guarded_move_inspection(cancelled_inspection),
            "cancelled inspection cannot commit"
        );
        assert!(
            runtime.latest_guarded_move_inspection(attempt).is_none(),
            "cancelling a newer inspection invalidates the older completed ticket"
        );
        assert_eq!(
            runtime.retry_verified_guarded_move_items(attempt, second_inspection.ticket),
            Err(RetryError::NotRetryable)
        );
        let mut final_inspection = runtime.begin_guarded_move_inspection(attempt).unwrap();
        final_inspection.candidates[0].source_matches = true;
        final_inspection.candidates[0].destination_parent_matches = true;
        final_inspection.candidates[0].destination =
            super::super::model::GuardedMoveDestinationState::Absent;
        let final_ticket = final_inspection.ticket;
        assert!(runtime.finish_guarded_move_inspection(final_inspection));
        let original_report = runtime.service.job(attempt.job_id()).unwrap().attempts[0]
            .terminal
            .clone()
            .unwrap();
        let failed_receipt = original_report.receipts[0].clone();
        assert_eq!(
            runtime.verified_guarded_move_retry_items(attempt, final_ticket),
            Some(vec![ItemId(0)]),
            "the exact no-effect MoveReceipt is safe only alongside its direct proof"
        );
        let mut conflicting = failed_receipt.clone();
        conflicting.outcome = super::super::model::ItemOutcome::CancelledBeforeStart;
        let mut wrong_source_path = failed_receipt.clone();
        wrong_source_path.move_receipt.as_mut().unwrap().source = PathBuf::from("other.txt");
        let mut wrong_destination_path = failed_receipt.clone();
        wrong_destination_path
            .move_receipt
            .as_mut()
            .unwrap()
            .destination = PathBuf::from("other-target/source.txt");
        let mut destination_published = failed_receipt.clone();
        destination_published
            .move_receipt
            .as_mut()
            .unwrap()
            .destination_published = true;
        let mut source_removed = failed_receipt.clone();
        source_removed.move_receipt.as_mut().unwrap().source_removed = true;
        let mut partial_receipt = failed_receipt.clone();
        partial_receipt.outcome = super::super::model::ItemOutcome::Partial;
        let mut unknown_receipt = failed_receipt.clone();
        unknown_receipt.outcome = super::super::model::ItemOutcome::Unknown;
        let rejected_receipts = [
            ("missing receipt", Vec::new()),
            (
                "duplicate receipt",
                vec![failed_receipt.clone(), failed_receipt.clone()],
            ),
            (
                "conflicting duplicate receipt",
                vec![failed_receipt.clone(), conflicting],
            ),
            ("wrong source path", vec![wrong_source_path]),
            ("wrong destination path", vec![wrong_destination_path]),
            ("published destination", vec![destination_published]),
            ("removed source", vec![source_removed]),
            ("partial item outcome", vec![partial_receipt]),
            ("unknown item outcome", vec![unknown_receipt]),
        ];
        for (case, receipts) in rejected_receipts {
            let mut report = original_report.clone();
            report.receipts = receipts;
            assert!(
                runtime
                    .service
                    .replace_terminal_report_for_test(attempt, report.clone())
            );
            assert_eq!(
                runtime.retry_verified_guarded_move_items(attempt, final_ticket),
                Err(RetryError::NotRetryable),
                "{case}"
            );
            let snapshot = runtime.service.job(attempt.job_id()).unwrap();
            assert_eq!(
                snapshot.attempts.len(),
                1,
                "{case} must not create an attempt"
            );
            assert_eq!(
                snapshot.attempts[0].terminal.as_ref(),
                Some(&report),
                "{case} leaves its terminal report untouched"
            );
            assert!(
                runtime
                    .service
                    .replace_terminal_report_for_test(attempt, original_report.clone())
            );
        }
        let mut not_allowed_report = original_report.clone();
        not_allowed_report.retry = RetryPlan::NotAllowed;
        assert!(
            runtime
                .service
                .replace_terminal_report_for_test(attempt, not_allowed_report)
        );
        assert_eq!(
            runtime.retry_verified_guarded_move_items(attempt, final_ticket),
            Err(RetryError::NotRetryable)
        );
        let mut succeeded_report = original_report.clone();
        succeeded_report.outcome = TerminalOutcome::Succeeded;
        assert!(
            runtime
                .service
                .replace_terminal_report_for_test(attempt, succeeded_report)
        );
        assert_eq!(
            runtime.retry_verified_guarded_move_items(attempt, final_ticket),
            Err(RetryError::NotRetryable)
        );
        assert!(
            runtime
                .service
                .replace_terminal_report_for_test(attempt, original_report.clone())
        );
        let retry = runtime
            .retry_verified_guarded_move_items(attempt, final_ticket)
            .unwrap();
        assert_ne!(retry, attempt);
        assert_eq!(
            runtime.retry_verified_guarded_move_items(attempt, final_ticket),
            Err(RetryError::NotRetryable)
        );
        let job = runtime.service.job(attempt.job_id()).unwrap();
        assert_eq!(job.attempts.len(), 2);
        assert_eq!(
            job.attempts[0].terminal,
            Some(original_report),
            "terminal attempt remains immutable"
        );
        assert_eq!(
            job.attempts[0].terminal.as_ref().unwrap().retry,
            RetryPlan::RequiresReinspection
        );
        assert_eq!(job.attempts[1].retry_items, vec![ItemId(0)]);
    }

    #[test]
    fn copy_failure_evidence_is_exact_and_inspection_is_worker_return_gated_and_latest_ticketed() {
        use crate::operations::model::{
            CopyFailureInspectionCandidate, CopyFailureInspectionResult, CopyInspectionState,
            CopyPublicationState, CopySourceSignatureSemantics,
        };
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let attempt = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let source = PathBuf::from("source.txt");
        let destination = PathBuf::from("target/source.txt");
        let provenance = super::super::model::CopyFailureProvenance {
            item: ItemId(0),
            source: source.clone(),
            destination: destination.clone(),
            publication: CopyPublicationState::Published,
            source_identity: Some(
                crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                    "source-id".into(),
                ),
            ),
            source_signature: Some(super::super::model::SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 10,
            }),
            source_signature_semantics:
                CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
            destination_parent_identity: None,
            stage: None,
        };
        let mut wrong_path = provenance.clone();
        wrong_path.destination = PathBuf::from("target/other.txt");
        assert!(
            runtime
                .commit_fact(ExecutorFact::CopyFailureProvenance {
                    token: attempt,
                    provenance: wrong_path
                })
                .is_none(),
            "wrong destination path evidence is rejected"
        );
        let fact = ExecutorFact::CopyFailureProvenance {
            token: attempt,
            provenance: provenance.clone(),
        };
        assert!(runtime.commit_fact(fact.clone()).is_none());
        assert!(
            runtime.commit_fact(fact).is_none(),
            "duplicate item evidence is rejected"
        );
        let mut terminal = report();
        let directory_evidence = super::super::model::CopyDirectoryStageEvidence {
            stage_path_units: "stage".encode_utf16().collect(),
            destination_path_units: "destination".encode_utf16().collect(),
            expected_root_identity: "forged-stage-root-identity".into(),
            phase: super::super::model::CopyDirectoryStagePhase::CreatedRootIdentityUnverified,
            detail: "stage root identity query failed".into(),
        };
        terminal.receipts[0].detail = Some(format!("{}{}",
            super::super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX,
            serde_json::to_string(&directory_evidence).unwrap()));
        let expected_terminal = terminal.clone();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: attempt,
                report: terminal.clone(),
            })
            .unwrap();
        assert!(
            runtime.begin_copy_failure_inspection(attempt).is_none(),
            "terminal alone cannot start inspection"
        );
        runtime.mark_operation_worker_returned(attempt);
        let first = runtime.begin_copy_failure_inspection(attempt).unwrap();
        assert_eq!(first.directory_candidates.len(), 1);
        assert_eq!(first.directory_candidates[0].evidence,
            super::super::model::CopyDirectoryInspectionEvidence::RetainedStage(directory_evidence.clone()));
        let second = runtime.begin_copy_failure_inspection(attempt).unwrap();
        let make_result = |work: &CopyFailureInspectionResult| CopyFailureInspectionResult {
            ticket: work.ticket,
            attempt: work.attempt,
            candidates: work
                .candidates
                .iter()
                .map(|candidate| CopyFailureInspectionCandidate {
                    provenance: candidate.provenance.clone(),
                    source: CopyInspectionState::Changed,
                    destination_parent: CopyInspectionState::Indeterminate,
                    destination: CopyInspectionState::Absent,
                    stage: CopyInspectionState::Indeterminate,
                    stage_content: CopyInspectionState::Indeterminate,
                })
                .collect(),
            directory_candidates: work.directory_candidates.iter().cloned().map(|mut candidate| {
                candidate.stage_root = CopyInspectionState::Matches;
                candidate.destination_root = CopyInspectionState::Changed;
                candidate
            }).collect(),
        };
        assert!(
            !runtime.finish_copy_failure_inspection(make_result(&first)),
            "superseded ticket cannot commit"
        );
        assert!(runtime.finish_copy_failure_inspection(make_result(&second)));
        assert!(
            !runtime.finish_copy_failure_inspection(make_result(&second)),
            "duplicate result cannot commit"
        );
        assert_eq!(
            runtime.copy_failure_provenance(attempt).unwrap(),
            vec![provenance]
        );
        let job = runtime.job(attempt).unwrap();
        assert_eq!(job.attempts[0].terminal.as_ref(), Some(&expected_terminal));
        let latest = runtime
            .latest_copy_failure_inspection(attempt)
            .unwrap()
            .1
            .unwrap();
        assert_eq!(
            latest.candidates[0].destination,
            CopyInspectionState::Absent
        );
        assert_eq!(latest.directory_candidates[0].stage_root, CopyInspectionState::Indeterminate,
            "Runtime must not upgrade an unverified created-root identity");
        assert_eq!(latest.directory_candidates[0].destination_root, CopyInspectionState::Changed,
            "normalization applies only to the stage-root observation");
        let cancelled = runtime.begin_copy_failure_inspection(attempt).unwrap();
        assert!(runtime.cancel_copy_failure_inspection(cancelled.ticket));
        assert!(
            !runtime.finish_copy_failure_inspection(make_result(&cancelled)),
            "cancelled inspection is harmless"
        );
    }

    #[test]
    fn verified_copy_retry_carries_exact_inspected_worker_guard() {
        use super::super::model::{
            CopyFailureInspectionCandidate, CopyFailureInspectionResult, CopyPublicationState,
            CopyRetryGuard, CopySourceSignatureSemantics,
        };
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let attempt = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let source = PathBuf::from("source.txt");
        let destination = PathBuf::from("target/source.txt");
        let source_identity =
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(10, 20);
        let parent_identity =
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(30, 40);
        let provenance = super::super::model::CopyFailureProvenance {
            item: ItemId(0),
            source: source.clone(),
            destination: destination.clone(),
            publication: CopyPublicationState::NotPublished,
            source_identity: Some(source_identity.clone()),
            source_signature: Some(super::super::model::SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 12,
            }),
            source_signature_semantics:
                CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
            destination_parent_identity: Some(parent_identity.clone()),
            stage: None,
        };
        runtime.commit_fact(ExecutorFact::CopyFailureProvenance {
            token: attempt,
            provenance: provenance.clone(),
        });
        let mut terminal = report();
        terminal.outcome = TerminalOutcome::Partial;
        terminal.receipts[0].outcome = super::super::model::ItemOutcome::Partial;
        terminal.retry = RetryPlan::RequiresReinspection;
        let original_terminal = terminal.clone();
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: attempt,
                report: terminal,
            })
            .unwrap();
        runtime.mark_operation_worker_returned(attempt);
        let inspection = runtime.begin_copy_failure_inspection(attempt).unwrap();
        let finished = CopyFailureInspectionResult {
            ticket: inspection.ticket,
            attempt,
            candidates: vec![CopyFailureInspectionCandidate {
                provenance,
                source: super::super::model::CopyInspectionState::Changed,
                destination_parent: super::super::model::CopyInspectionState::Matches,
                destination: super::super::model::CopyInspectionState::Absent,
                stage: super::super::model::CopyInspectionState::Absent,
                stage_content: super::super::model::CopyInspectionState::Absent,
            }],
            directory_candidates: vec![],
        };
        assert!(runtime.finish_copy_failure_inspection(finished));
        assert_eq!(
            runtime.verified_copy_retry_guards(attempt, inspection.ticket),
            Some(Vec::new()),
            "changed source observation cannot authorize retry"
        );
        let fresh_inspection = runtime.begin_copy_failure_inspection(attempt).unwrap();
        assert!(runtime.finish_copy_failure_inspection(
            super::super::model::CopyFailureInspectionResult {
                ticket: fresh_inspection.ticket,
                attempt,
                candidates: vec![super::super::model::CopyFailureInspectionCandidate {
                    provenance: super::super::model::CopyFailureProvenance {
                        item: ItemId(0),
                        source: PathBuf::from("source.txt"),
                        destination: PathBuf::from("target/source.txt"),
                        publication: super::super::model::CopyPublicationState::NotPublished,
                        source_identity: Some(
                            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                                10, 20,
                            ),
                        ),
                        source_signature: Some(
                            super::super::model::SourceCleanupLeafSignature {
                                number_of_links: 1,
                                change_time: 12,
                            },
                        ),
                        source_signature_semantics: super::super::model::CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
                        destination_parent_identity: Some(
                            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                                30, 40,
                            ),
                        ),
                        stage: None,
                    },
                    source: super::super::model::CopyInspectionState::Matches,
                    destination_parent: super::super::model::CopyInspectionState::Matches,
                    destination: super::super::model::CopyInspectionState::Absent,
                    stage: super::super::model::CopyInspectionState::Absent,
                    stage_content: super::super::model::CopyInspectionState::Absent,
                }],
                directory_candidates: vec![],
            }
        ));
        assert!(
            runtime
                .verified_copy_retry_guards(attempt, inspection.ticket)
                .is_none(),
            "a superseded ticket cannot authorize Copy retry"
        );
        let expected_guard = CopyRetryGuard {
            item: ItemId(0),
            source,
            destination,
            source_identity,
            source_signature: super::super::model::SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 12,
            },
            destination_parent_identity: parent_identity,
        };
        assert_eq!(
            runtime.verified_copy_retry_guards(attempt, fresh_inspection.ticket),
            Some(vec![expected_guard.clone()])
        );
        let retry = runtime
            .retry_verified_copy_items(attempt, fresh_inspection.ticket)
            .unwrap();
        assert_ne!(retry, attempt);
        assert_eq!(
            runtime.job(attempt).unwrap().attempts[0].terminal,
            Some(original_terminal)
        );
        let start = runtime.start_next().unwrap();
        let ExecutorCommand::Prepare {
            token,
            copy_retry_guards,
            retry_items,
            ..
        } = start.command
        else {
            panic!("verified Copy retry should reach the worker")
        };
        assert_eq!(token, retry);
        assert_eq!(retry_items, vec![ItemId(0)]);
        assert_eq!(copy_retry_guards, vec![expected_guard]);
    }

    #[test]
    fn copy_stage_cleanup_requires_latest_ticket_and_consumes_capability_once() {
        use super::super::model::{
            CopyFailureInspectionCandidate, CopyFailureInspectionResult, CopyFailureProvenance,
            CopyPublicationState, CopySourceSignatureSemantics, CopyStageObservation,
        };
        let root = tempfile::tempdir().unwrap();
        let stage_path = root.path().join("retained.stage");
        std::fs::write(&stage_path, b"retained stage bytes").unwrap();
        let handle = std::fs::OpenOptions::new().read(true).open(&stage_path).unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        runtime.run_id = Some([7; 16]);
        let source = PathBuf::from("source.txt");
        let destination = PathBuf::from("target/source.txt");
        let stage_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(10, 21);
        let parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(30, 40);
        let token = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let provenance = CopyFailureProvenance {
            item: ItemId(0),
            source: source.clone(),
            destination: destination.clone(),
            publication: CopyPublicationState::NotPublished,
            source_identity: Some(crate::filesystem_identity::FilesystemIdentity::from_windows_parts(10, 20)),
            source_signature: Some(super::super::model::SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 12,
            }),
            source_signature_semantics: CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
            destination_parent_identity: Some(parent_identity.clone()),
            stage: Some(CopyStageObservation {
                path: stage_path.clone(),
                identity: Some(stage_identity.clone()),
            }),
        };
        runtime.commit_fact(ExecutorFact::CopyFailureProvenance {
            token,
            provenance: provenance.clone(),
        });
        let mut terminal = report();
        terminal.outcome = TerminalOutcome::Failed;
        terminal.receipts[0].outcome = ItemOutcome::Failed;
        terminal.retry = RetryPlan::RequiresReinspection;
        let original_terminal = terminal.clone();
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal }).unwrap();
        runtime.mark_operation_worker_returned(token);
        assert!(runtime.adopt_returned_copy_stage_custody(CopyStageCustody {
            token,
            item: ItemId(0),
            path: stage_path.clone(),
            identity: stage_identity,
            parent_identity,
            handle,
        }));
        let inspection = runtime.begin_copy_failure_inspection(token).unwrap();
        let candidate = CopyFailureInspectionCandidate {
            provenance,
            source: super::super::model::CopyInspectionState::Matches,
            destination_parent: super::super::model::CopyInspectionState::Matches,
            destination: super::super::model::CopyInspectionState::Absent,
            stage: super::super::model::CopyInspectionState::Matches,
            stage_content: super::super::model::CopyInspectionState::Indeterminate,
        };
        assert!(runtime.finish_copy_failure_inspection(CopyFailureInspectionResult {
            ticket: inspection.ticket,
            attempt: token,
            candidates: vec![candidate.clone()],
            directory_candidates: vec![],
        }));
        assert_eq!(
            runtime.latest_copy_failure_inspection(token).unwrap().1.unwrap().candidates[0].stage_content,
            super::super::model::CopyInspectionState::Indeterminate,
            "held-handle custody does not compare stage bytes",
        );
        assert_eq!(runtime.verified_copy_stage_cleanup_items(token, inspection.ticket), Some(vec![ItemId(0)]));
        let newer = runtime.begin_copy_failure_inspection(token).unwrap();
        assert!(runtime.verified_copy_stage_cleanup_items(token, inspection.ticket).is_none());
        assert!(runtime.finish_copy_failure_inspection(CopyFailureInspectionResult {
            ticket: newer.ticket,
            attempt: token,
            candidates: vec![candidate],
            directory_candidates: vec![],
        }));
        assert_eq!(runtime.verified_copy_stage_cleanup_items(token, newer.ticket), Some(vec![ItemId(0)]));
        let cleanup = runtime.cleanup_verified_copy_stage(token, newer.ticket, ItemId(0)).unwrap();
        assert!(runtime.cleanup_verified_copy_stage(token, newer.ticket, ItemId(0)).is_err());
        assert!(runtime.verified_copy_stage_cleanup_items(token, newer.ticket).unwrap().is_empty());
        assert_eq!(runtime.job(token).unwrap().attempts[0].terminal, Some(original_terminal));
        let worker = runtime.start_next().unwrap();
        assert_eq!(worker.copy_stage_cleanup_lease.as_ref().unwrap().token, token);
        assert!(matches!(worker.command, ExecutorCommand::Prepare { token: actual, .. } if actual == cleanup));
        let running_ack = worker.copy_stage_cleanup_running_ack.unwrap();
        let mut mismatched_running = runtime.journal_attempt(cleanup).unwrap();
        mismatched_running.key.push_str(":mismatch");
        assert!(!runtime.resolve_copy_stage_cleanup_running_ack(
            cleanup,
            &mismatched_running,
            true,
        ));
        assert!(running_ack.try_recv().is_err(), "mismatched snapshots grant no cleanup authority");
        assert!(
            !runtime.resolve_copy_stage_cleanup_running_ack(
                cleanup,
                &runtime.journal_attempt(cleanup).unwrap(),
                true,
            ),
            "the one-shot ACK cannot be replayed"
        );
        assert!(stage_path.exists(), "rejected ACK preserves the held stage");
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: cleanup,
                report: report(),
            })
            .expect("cleanup terminalizes after the rejected Running ACK");
        runtime.mark_operation_worker_returned(cleanup);
        let late_running = runtime.journal_attempt(cleanup).unwrap();
        assert_eq!(late_running.state, super::super::journal::JournalState::Terminal);
        assert!(
            !runtime.resolve_copy_stage_cleanup_running_ack(cleanup, &late_running, true),
            "a late successful Running resolution cannot authorize a terminal cleanup"
        );
        assert!(stage_path.exists(), "terminal cleanup retains the rejected stage");
    }

    #[test]
    fn cancelling_queued_copy_stage_cleanup_drops_lease_without_touching_stage() {
        use super::super::model::{
            CopyFailureInspectionCandidate, CopyFailureInspectionResult, CopyFailureProvenance,
            CopyPublicationState, CopySourceSignatureSemantics, CopyStageObservation,
        };
        let root = tempfile::tempdir().unwrap();
        let stage_path = root.path().join("retained.stage");
        std::fs::write(&stage_path, b"original bytes").unwrap();
        let handle = std::fs::OpenOptions::new().read(true).write(true).open(&stage_path).unwrap();
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        runtime.run_id = Some([7; 16]);
        let source = PathBuf::from("source.txt");
        let destination = PathBuf::from("target/source.txt");
        let stage_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(10, 21);
        let parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(30, 40);
        let token = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let provenance = CopyFailureProvenance {
            item: ItemId(0), source: source.clone(), destination: destination.clone(),
            publication: CopyPublicationState::NotPublished,
            source_identity: Some(crate::filesystem_identity::FilesystemIdentity::from_windows_parts(10, 20)),
            source_signature: Some(super::super::model::SourceCleanupLeafSignature { number_of_links: 1, change_time: 12 }),
            source_signature_semantics: CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
            destination_parent_identity: Some(parent_identity.clone()),
            stage: Some(CopyStageObservation { path: stage_path.clone(), identity: Some(stage_identity.clone()) }),
        };
        runtime.commit_fact(ExecutorFact::CopyFailureProvenance { token, provenance: provenance.clone() });
        let original_terminal = super::super::model::TerminalReport {
            outcome: TerminalOutcome::Failed, final_counters: super::super::model::ProgressCounters::default(),
            receipts: vec![super::super::model::ItemReceipt { item: ItemId(0), outcome: super::super::model::ItemOutcome::Failed,
                detail: None, created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None }],
            retry: super::super::model::RetryPlan::RequiresReinspection, change_hints: Vec::new(),
        };
        runtime.commit_fact(ExecutorFact::Terminal { token, report: original_terminal.clone() }).unwrap();
        runtime.mark_operation_worker_returned(token);
        assert!(runtime.adopt_returned_copy_stage_custody(CopyStageCustody {
            token, item: ItemId(0), path: stage_path.clone(), identity: stage_identity,
            parent_identity, handle,
        }));
        let inspection = runtime.begin_copy_failure_inspection(token).unwrap();
        assert!(runtime.finish_copy_failure_inspection(CopyFailureInspectionResult {
            ticket: inspection.ticket, attempt: token,
            candidates: vec![CopyFailureInspectionCandidate { provenance, source: super::super::model::CopyInspectionState::Matches,
                destination_parent: super::super::model::CopyInspectionState::Matches,
                destination: super::super::model::CopyInspectionState::Absent, stage: super::super::model::CopyInspectionState::Matches,
                stage_content: super::super::model::CopyInspectionState::Indeterminate }],
            directory_candidates: vec![],
        }));
        let cleanup = runtime.cleanup_verified_copy_stage(token, inspection.ticket, ItemId(0)).unwrap();
        assert!(runtime.copy_stage_cleanup_leases.contains_key(&cleanup));
        let cancelled = runtime.cancel(cleanup).expect("queued cleanup cancellation terminalizes");
        assert_eq!(cancelled.event, ServiceEvent::TerminalCommitted { token: cleanup,
            report: runtime.job(cleanup).unwrap().attempts.last().unwrap().terminal.clone().unwrap() });
        assert!(!runtime.copy_stage_cleanup_leases.contains_key(&cleanup));
        assert_eq!(std::fs::read(&stage_path).unwrap(), b"original bytes");
        let mut reopened = std::fs::OpenOptions::new().read(true).write(true).open(&stage_path).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        reopened.seek(SeekFrom::Start(0)).unwrap();
        reopened.write_all(b"changed bytes").unwrap();
        drop(reopened);
        std::fs::remove_file(&stage_path).unwrap();
        assert_eq!(runtime.job(token).unwrap().attempts[0].terminal, Some(original_terminal));
    }

    #[test]
    fn terminal_attempt_b_supersedes_pending_move_inspection_for_a() {
        let mut runtime = OperationRuntime::new(OperationService::new(4));
        let attempt_a = runtime.submit(pending_move_request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let source_identity =
            crate::filesystem_identity::FilesystemIdentity::from_fallback_token("source".into());
        let parent_identity = crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
            "destination-at-submit".into(),
        );
        runtime.commit_fact(ExecutorFact::MoveGuardResolved {
            token: attempt_a,
            guard: MoveIdentityGuard {

                source_identities: vec![source_identity.clone()],
                destination_identity: parent_identity.clone(),
                 },
        });
        runtime.commit_fact(ExecutorFact::GuardedMoveNoEffectProof {
            token: attempt_a,
            proof: super::super::model::GuardedMoveNoEffectProof {
                item: ItemId(0),
                source: PathBuf::from("source.txt"),
                destination: PathBuf::from("target/source.txt"),
                source_identity,
                destination_parent_identity: parent_identity,
            },
        });
        let mut report_a = report();
        report_a.outcome = TerminalOutcome::Failed;
        report_a.retry = RetryPlan::Explicit(vec![ItemId(0)]);
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: attempt_a,
                report: report_a,
            })
            .unwrap();
        runtime.mark_operation_worker_returned(attempt_a);
        let stale_a = runtime.begin_guarded_move_inspection(attempt_a).unwrap();

        let attempt_b = runtime.retry(attempt_a).unwrap();
        runtime.start_next().unwrap();
        let mut report_b = report();
        report_b.outcome = TerminalOutcome::Failed;
        report_b.retry = RetryPlan::RequiresReinspection;
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token: attempt_b,
                report: report_b,
            })
            .unwrap();
        assert!(
            !runtime.finish_guarded_move_inspection(stale_a),
            "old A result cannot commit after B becomes latest"
        );
        assert!(
            runtime
                .guarded_move_inspection(super::super::model::InspectionTicket::new(1))
                .is_none()
        );
    }

    #[test]
    fn accepted_copy_terminal_is_the_only_source_of_copy_undo_receipts() {
        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let receipt = super::super::model::CopyUndoReceipt {
            destination: PathBuf::from("target/source.txt"),
            parent_identity: "1:0000000000000002".into(),
            file_identity: "1:0000000000000003".into(),
            content_sha256: "0123456789abcdef".into(),
            leaf_signature: super::super::model::SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 42,
            },
        };
        let report = TerminalReport {
            outcome: TerminalOutcome::Succeeded,
            final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt {
                item: ItemId(0),
                outcome: ItemOutcome::Completed,
                detail: None,
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: Some(receipt.clone()),
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: vec![],
        };
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: report.clone(),
            })
            .expect("exact terminal should commit");
        assert!(
            runtime
                .commit_fact(ExecutorFact::Terminal { token, report })
                .is_none(),
            "duplicate terminal must not rewrite the undo ledger"
        );
        assert!(
            runtime
                .commit_fact(ExecutorFact::Progress {
                    token,
                    update: ProgressUpdate {
                        sequence: 1,
                        counters: ProgressCounters::default(),
                        current_item: Some(ItemId(0)),
                    },
                })
                .is_none(),
            "late progress must not change terminal undo state"
        );

        let (reservation, entry) = runtime.reserve_undo().unwrap();
        let UndoEntry::Copy(reserved) = entry else {
            panic!("accepted Copy output proof must be the undo entry");
        };
        assert_eq!(reserved, receipt);
        assert!(runtime.finish_trash_undo(reservation, false));
        let (_, retry_entry) = runtime.reserve_undo().unwrap();
        assert!(matches!(retry_entry, UndoEntry::Copy(retry) if retry == receipt));
    }

    #[test]
    fn created_object_undo_requires_one_matching_candidate_and_accepted_success() {
        use super::super::model::{CreatedObjectUndoKind, CreatedObjectUndoReceipt};

        let parent = PathBuf::from("target");
        for (kind, created_kind, path, detail, folder_path) in [
            (OperationKind::CreateFile, CreatedObjectUndoKind::EmptyFile {
                content_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                signature: super::super::model::SourceCleanupLeafSignature { number_of_links: 1, change_time: 7 },
            }, parent.join("New File.txt"), Some("New File.txt".to_owned()), None),
            (OperationKind::CreateFolder, CreatedObjectUndoKind::EmptyFolder,
                parent.join("New Folder"), Some("target\\New Folder".to_owned()), Some(parent.join("New Folder"))),
        ] {
            let mut runtime = OperationRuntime::default();
            let mut request = request("unused-source");
            request.kind = kind;
            request.sources.clear();
            request.destination = Some(parent.clone());
            request.create_parent_directory_identity_guard = Some(crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2));
            let token = runtime.submit(request).unwrap();
            runtime.start_next().unwrap();
            let proof = CreatedObjectUndoReceipt {
                destination: path.clone(), parent_identity: "1:0000000000000002".into(),
                object_identity: "object-id".into(), kind: created_kind,
            };
            runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
            let report = TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt { item: ItemId(0), outcome: ItemOutcome::Completed,
                    detail, created_folder_path: folder_path, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None }],
                retry: RetryPlan::NotAllowed,
                change_hints: vec![],
            };
            runtime.commit_fact(ExecutorFact::Terminal { token, report: report.clone() }).expect("accepted create terminal");
            assert!(runtime.commit_fact(ExecutorFact::Terminal { token, report }).is_none(), "duplicate terminal cannot add another receipt");
            let (_, undo) = runtime.reserve_undo().expect("exact accepted create proof is undoable");
            assert!(matches!(undo, UndoEntry::CreatedObject(receipt) if receipt == proof));
            assert!(runtime.reserve_undo().is_none());
        }
    }

    #[test]
    fn shortcut_created_object_undo_requires_exact_request_and_single_proof() {
        use super::super::model::{
            CreatedObjectUndoKind, CreatedObjectUndoReceipt, ShortcutCreateGuard,
            SourceCleanupLeafSignature,
        };

        let source = PathBuf::from(r"C:\items\レポート.txt");
        let parent = source.parent().unwrap().to_path_buf();
        let output = PathBuf::from(r"C:\items\レポート.txt.lnk");
        let guard = ShortcutCreateGuard {
            source_kind: crate::directory::EntryKind::File,
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 20),
            parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
        };
        let request = OperationRequest::create_shortcut(source.clone(), parent.clone(), guard).unwrap();

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(request.clone()).expect("captured shortcut request is admitted");
        runtime.start_next().unwrap();
        let proof = CreatedObjectUndoReceipt {
            destination: output.clone(),
            parent_identity: "1:0000000000000002".into(),
            object_identity: "1:0000000000000015".into(),
            kind: CreatedObjectUndoKind::ShortcutFile {
                content_sha256: "a".repeat(64),
                signature: SourceCleanupLeafSignature { number_of_links: 1, change_time: 7 },
                size: 512,
            },
        };
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(0), outcome: ItemOutcome::Completed,
                    detail: Some(output.display().to_string()),
                    created_folder_path: None, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: vec![],
            },
        }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "duplicate shortcut proof cannot create Undo authority");

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(0), outcome: ItemOutcome::Completed,
                    detail: Some(output.display().to_string()),
                    created_folder_path: None, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: vec![],
            },
        }).unwrap();
        assert!(matches!(runtime.reserve_undo(), Some((_, UndoEntry::CreatedObject(receipt))) if receipt == proof));

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(OperationRequest::create_shortcut(
            PathBuf::from(r"C:\items\レポート.txt"),
            PathBuf::from(r"C:\items"),
            ShortcutCreateGuard {
                source_kind: crate::directory::EntryKind::File,
                source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 20),
                parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            },
        ).unwrap()).unwrap();
        runtime.start_next().unwrap();
        let mut forged = proof;
        forged.destination = PathBuf::from(r"C:\items\other.lnk");
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: forged });
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(0), outcome: ItemOutcome::Completed,
                    detail: Some(output.display().to_string()),
                    created_folder_path: None, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: vec![],
            },
        }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "a forged destination proof cannot be admitted");
    }

    #[test]
    fn shortcut_request_shape_rejects_foreign_and_malformed_guards() {
        use super::super::model::ShortcutCreateGuard;
        let source = PathBuf::from(r"C:\items\file.txt");
        let guard = ShortcutCreateGuard {
            source_kind: crate::directory::EntryKind::File,
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 20),
            parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
        };
        let mut runtime = OperationRuntime::default();
        let mut foreign = request("unused-source");
        foreign.shortcut_create_guard = Some(guard.clone());
        assert!(matches!(runtime.submit(foreign), Err(super::SubmitError::InvalidRequest)));

        let mut malformed = OperationRequest::create_shortcut(
            source.clone(), source.parent().unwrap().to_path_buf(), guard.clone(),
        ).unwrap();
        malformed.sources.push(PathBuf::from(r"C:\items\other.txt"));
        assert!(matches!(runtime.submit(malformed), Err(super::SubmitError::InvalidRequest)));

        let wrong_parent = OperationRequest::create_shortcut(
            source, PathBuf::from(r"C:\other"), guard,
        );
        assert!(wrong_parent.is_err());
    }

    #[test]
    fn created_object_undo_rejects_conflicting_or_failed_terminal_candidates() {
        use super::super::model::{CreatedObjectUndoKind, CreatedObjectUndoReceipt};
        let mut runtime = OperationRuntime::default();
        let token = runtime.submit({
            let mut request = request("unused-source");
            request.kind = OperationKind::CreateFile;
            request.sources.clear();
            request.destination = Some(PathBuf::from("target"));
            request.create_parent_directory_identity_guard = Some(crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2));
            request
        }).unwrap();
        runtime.start_next().unwrap();
        let proof = |id: &str| CreatedObjectUndoReceipt {
            destination: PathBuf::from("target/New File.txt"), parent_identity: "1:0000000000000002".into(),
            object_identity: id.into(), kind: CreatedObjectUndoKind::EmptyFile {
                content_sha256: "empty".into(),
                signature: super::super::model::SourceCleanupLeafSignature { number_of_links: 1, change_time: 1 },
            },
        };
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: proof("one") });
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate { token, item: ItemId(0), receipt: proof("two") });
        let report = TerminalReport {
            outcome: TerminalOutcome::Succeeded, final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt { item: ItemId(0), outcome: ItemOutcome::Completed, detail: Some("New File.txt".into()),
                created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None }],
            retry: RetryPlan::NotAllowed, change_hints: vec![],
        };
        runtime.commit_fact(ExecutorFact::Terminal { token, report }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "conflicting proof facts invalidate the candidate");
    }

    #[test]
    fn created_object_candidate_after_failed_terminal_cannot_enter_undo() {
        use super::super::model::{CreatedObjectUndoKind, CreatedObjectUndoReceipt};

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit({
            let mut request = request("unused-source");
            request.kind = OperationKind::CreateFile;
            request.sources.clear();
            request.destination = Some(PathBuf::from("target"));
            request.create_parent_directory_identity_guard = Some(
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            );
            request
        }).unwrap();
        runtime.start_next().unwrap();
        let candidate = CreatedObjectUndoReceipt {
            destination: PathBuf::from("target/New File.txt"),
            parent_identity: "1:0000000000000002".into(),
            object_identity: "created-object".into(),
            kind: CreatedObjectUndoKind::EmptyFile {
                content_sha256: "empty".into(),
                signature: super::super::model::SourceCleanupLeafSignature {
                    number_of_links: 1,
                    change_time: 1,
                },
            },
        };
        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate {
            token,
            item: ItemId(0),
            receipt: candidate.clone(),
        });
        runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Failed,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(0),
                    outcome: ItemOutcome::Failed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: vec![],
            },
        }).unwrap();

        runtime.commit_fact(ExecutorFact::CreatedObjectUndoCandidate {
            token,
            item: ItemId(0),
            receipt: candidate,
        });
        assert!(runtime.reserve_undo().is_none(), "late proof after a failed terminal is stale");
    }

    #[test]
    fn move_undo_requires_exact_candidate_and_accepted_completed_terminal() {
        use super::super::model::{MoveIdentityGuard, MoveUndoReceipt, SourceCleanupParentIdentity};

        let source = PathBuf::from(r"C:\source\x");
        let destination = PathBuf::from(r"C:\destination\x");
        let source_parent = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 11);
        let destination_parent = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 12);
        let object = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 13);
        let proof = MoveUndoReceipt {
            source: source.clone(), destination: destination.clone(), object_identity: object.clone(),
            object_kind: super::super::model::MoveUndoObjectKind::Directory,
            source_parent_route: vec![SourceCleanupParentIdentity { path: source.parent().unwrap().to_path_buf(), identity: source_parent }],
            destination_parent_route: vec![SourceCleanupParentIdentity { path: destination.parent().unwrap().to_path_buf(), identity: destination_parent.clone() }],
            cross_volume_file: None,
        };
        let make_report = |outcome, item_outcome| TerminalReport {
            outcome, final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt { item: ItemId(0), outcome: item_outcome, detail: None,
                created_folder_path: None, created_archive_path: None,
                move_receipt: Some(super::super::model::MoveReceipt { source: source.clone(), destination: destination.clone(), destination_published: true, source_removed: true }),
                replace_receipt: None, copy_undo: None }],
            retry: RetryPlan::NotAllowed, change_hints: vec![],
        };

        let mut runtime = OperationRuntime::default();
        let mut first_request = request(source.to_string_lossy().as_ref());
        first_request.kind = OperationKind::Move;
        first_request.destination = Some(destination.parent().unwrap().to_path_buf());
        let source_two = PathBuf::from(r"C:\source\y");
        let destination_two = PathBuf::from(r"C:\destination\y");
        let object_two = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 14);
        first_request.sources.push(source_two.clone());
        first_request.move_identity_guard = Some(MoveIdentityGuard {
 source_identities: vec![object, object_two.clone()], destination_identity: proof.destination_parent_route[0].identity.clone(),
            });
        let token = runtime.submit(first_request).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        let mut proof_two = proof.clone();
        proof_two.source = source_two.clone();
        proof_two.destination = destination_two.clone();
        proof_two.object_identity = object_two;
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(1), receipt: proof_two.clone() });
        let mut report = make_report(TerminalOutcome::Partial, ItemOutcome::Completed);
        report.receipts.push(ItemReceipt { item: ItemId(1), outcome: ItemOutcome::Completed, detail: None,
            created_folder_path: None, created_archive_path: None,
            move_receipt: Some(super::super::model::MoveReceipt { source: source_two, destination: destination_two, destination_published: true, source_removed: true }),
            replace_receipt: None, copy_undo: None });
        runtime.commit_fact(ExecutorFact::Terminal { token, report }).expect("accepted partial batch terminal");
        assert!(runtime.commit_fact(ExecutorFact::Terminal { token, report: make_report(TerminalOutcome::Succeeded, ItemOutcome::Completed) }).is_none(), "duplicate terminal is inert");
        let (_, admitted) = runtime.reserve_undo().expect("accepted exact proof should enter LIFO once");
        assert!(matches!(admitted, UndoEntry::Move(receipts) if receipts == vec![proof.clone(), proof_two]));
        assert!(runtime.reserve_undo().is_none());

        let mut runtime = OperationRuntime::default();
        let mut unknown_request = request(source.to_string_lossy().as_ref());
        unknown_request.kind = OperationKind::Move;
        unknown_request.destination = Some(destination.parent().unwrap().to_path_buf());
        unknown_request.move_identity_guard = Some(MoveIdentityGuard {
 source_identities: vec![proof.object_identity.clone()], destination_identity: proof.destination_parent_route[0].identity.clone(),
            });
        let token = runtime.submit(unknown_request).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: make_report(TerminalOutcome::Unknown, ItemOutcome::Completed) }).unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        assert!(runtime.reserve_undo().is_none(), "unknown terminal and late candidate cannot authorize undo");

        let mut runtime = OperationRuntime::default();
        let mut duplicate_request = request(source.to_string_lossy().as_ref());
        duplicate_request.kind = OperationKind::Move;
        duplicate_request.destination = Some(destination.parent().unwrap().to_path_buf());
        duplicate_request.move_identity_guard = Some(MoveIdentityGuard {
 source_identities: vec![proof.object_identity.clone()], destination_identity: proof.destination_parent_route[0].identity.clone(),
            });
        let token = runtime.submit(duplicate_request).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proof.clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: make_report(TerminalOutcome::Succeeded, ItemOutcome::Completed) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "duplicate candidate invalidates that item");
    }

    #[test]
    fn cross_volume_retry_move_undo_requires_exact_retry_guard_and_nonempty_completed_set() {
        use super::super::model::{
            CrossVolumeMoveNoEffectProof, CrossVolumeMoveRetryGuard, CrossVolumeMoveUndoProof,
            ItemId, ItemReceipt, ItemOutcome, MoveReceipt, MoveUndoObjectKind, MoveUndoReceipt,
            OperationKind, SourceCleanupLeafSignature, SourceCleanupParentIdentity,
        };

        fn source(index: usize) -> PathBuf {
            PathBuf::from(if index == 0 { r"C:\source\a.bin" } else { r"C:\source\b.bin" })
        }
        fn destination(index: usize) -> PathBuf {
            PathBuf::from(if index == 0 { r"E:\destination\a.bin" } else { r"E:\destination\b.bin" })
        }
        fn signature(index: usize) -> SourceCleanupLeafSignature {
            SourceCleanupLeafSignature { number_of_links: 1, change_time: 100 + index as i64 }
        }
        fn undo_candidate(index: usize) -> MoveUndoReceipt {
            let source = source(index);
            let destination = destination(index);
            let source_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 13 + index as u64);
            let destination_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 21 + index as u64);
            let source_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 11);
            let destination_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 12);
            let leaf_signature = signature(index);
            MoveUndoReceipt {
                source: source.clone(),
                destination: destination.clone(),
                object_identity: destination_identity,
                object_kind: MoveUndoObjectKind::RegularFile,
                source_parent_route: vec![SourceCleanupParentIdentity {
                    path: source.parent().unwrap().to_path_buf(), identity: source_parent_identity,
                }],
                destination_parent_route: vec![SourceCleanupParentIdentity {
                    path: destination.parent().unwrap().to_path_buf(), identity: destination_parent_identity,
                }],
                cross_volume_file: Some(CrossVolumeMoveUndoProof {
                    original_source_identity: source_identity,
                    original_source_signature: leaf_signature,
                    destination_signature: leaf_signature,
                    content_sha256: format!("{:064x}", index + 1),
                    directory_manifest: None,
                }),
            }
        }
        fn start_retry() -> (OperationRuntime, AttemptToken, MoveUndoReceipt, MoveUndoReceipt) {
            let source_a = source(0);
            let source_b = source(1);
            let destination_parent = PathBuf::from(r"E:\destination");
            let mut operation = request(source_a.to_string_lossy().as_ref());
            operation.kind = OperationKind::Move;
            operation.sources = vec![source_a.clone(), source_b.clone()];
            operation.destination = Some(destination_parent.clone());
            let mut runtime = OperationRuntime::new(OperationService::new(4));
            let first = runtime.submit(operation).unwrap();
            runtime.start_next().unwrap();
            let source_identity_b = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 14);
            let destination_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 12);
            runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token: first, item: ItemId(0), receipt: undo_candidate(0) });
            runtime.commit_fact(ExecutorFact::CrossVolumeMoveNoEffectProof {
                token: first,
                proof: CrossVolumeMoveNoEffectProof {
                    item: ItemId(1), source: source_b.clone(), destination: destination(1),
                    source_identity: source_identity_b.clone(), source_signature: signature(1),
                    destination_parent_identity: destination_parent_identity.clone(),
                },
            });
            let first_report = TerminalReport {
                outcome: TerminalOutcome::Partial,
                final_counters: ProgressCounters::default(),
                receipts: vec![
                    ItemReceipt {
                        item: ItemId(0), outcome: ItemOutcome::Completed, detail: None,
                        created_folder_path: None, created_archive_path: None,
                        move_receipt: Some(MoveReceipt { source: source_a, destination: destination(0), destination_published: true, source_removed: true }),
                        replace_receipt: None, copy_undo: None,
                    },
                    ItemReceipt {
                        item: ItemId(1), outcome: ItemOutcome::Failed, detail: None,
                        created_folder_path: None, created_archive_path: None, move_receipt: None,
                        replace_receipt: None, copy_undo: None,
                    },
                ],
                retry: RetryPlan::RequiresReinspection,
                change_hints: vec![],
            };
            runtime.commit_fact(ExecutorFact::Terminal { token: first, report: first_report }).unwrap();
            runtime.mark_operation_worker_returned(first);
            let retry = runtime.service.retry_verified_cross_volume_move_items(first, vec![CrossVolumeMoveRetryGuard {
                proof: CrossVolumeMoveNoEffectProof {
                    item: ItemId(1), source: source_b, destination: destination(1),
                    source_identity: source_identity_b, source_signature: signature(1),
                    destination_parent_identity,
                },
            }]).unwrap();
            runtime.start_next().unwrap();
            (runtime, retry, undo_candidate(0), undo_candidate(1))
        }
        fn retry_terminal(items: &[(ItemId, ItemOutcome)]) -> TerminalReport {
            TerminalReport {
                outcome: if items.iter().all(|(_, outcome)| *outcome == ItemOutcome::Completed) { TerminalOutcome::Succeeded } else { TerminalOutcome::Partial },
                final_counters: ProgressCounters::default(),
                receipts: items.iter().map(|(item, outcome)| ItemReceipt {
                    item: *item, outcome: *outcome, detail: None, created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: (*outcome == ItemOutcome::Completed).then(|| MoveReceipt {
                        source: source(item.0 as usize), destination: destination(item.0 as usize),
                        destination_published: true, source_removed: true,
                    }),
                    replace_receipt: None, copy_undo: None,
                }).collect(),
                retry: RetryPlan::NotAllowed, change_hints: vec![],
            }
        }

        let (mut runtime, retry, _, candidate_b) = start_retry();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token: retry, item: ItemId(1), receipt: candidate_b.clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token: retry, report: retry_terminal(&[(ItemId(1), ItemOutcome::Completed)]) }).unwrap();
        assert!(matches!(runtime.undo_ledger.as_slice(), [UndoEntry::Move(previous), UndoEntry::Move(current)]
            if previous.len() == 1 && previous[0].source == source(0)
                && current.len() == 1 && current[0].source == source(1)),
            "the exact guarded retry candidate appends after the prior terminal's Undo entry");

        let (mut runtime, retry, candidate_a, _) = start_retry();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token: retry, item: ItemId(0), receipt: candidate_a });
        runtime.commit_fact(ExecutorFact::Terminal { token: retry, report: retry_terminal(&[
            (ItemId(0), ItemOutcome::Completed), (ItemId(1), ItemOutcome::Failed),
        ]) }).unwrap();
        assert!(matches!(runtime.undo_ledger.as_slice(), [UndoEntry::Move(previous)] if previous.len() == 1 && previous[0].source == source(0)),
            "a Completed receipt outside the retry set adds no Undo entry");

        let (mut runtime, retry, _, mut mismatched_b) = start_retry();
        mismatched_b.cross_volume_file.as_mut().unwrap().original_source_signature.change_time += 1;
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token: retry, item: ItemId(1), receipt: mismatched_b });
        runtime.commit_fact(ExecutorFact::Terminal { token: retry, report: retry_terminal(&[(ItemId(1), ItemOutcome::Completed)]) }).unwrap();
        assert!(matches!(runtime.undo_ledger.as_slice(), [UndoEntry::Move(previous)] if previous.len() == 1 && previous[0].source == source(0)),
            "a mismatched source signature cannot authorize retry Undo");

        let (mut runtime, retry, _, _) = start_retry();
        runtime.commit_fact(ExecutorFact::Terminal { token: retry, report: retry_terminal(&[(ItemId(1), ItemOutcome::Failed)]) }).unwrap();
        assert!(matches!(runtime.undo_ledger.as_slice(), [UndoEntry::Move(previous)] if previous.len() == 1 && previous[0].source == source(0)),
            "a failed retry with no Completed candidates creates no empty group or trims older history");
    }

    #[test]
    fn directory_copy_undo_requires_one_exact_attempt_candidate_and_success() {
        use super::super::model::{DirectoryCopyUndoReceipt, SourceCleanupParentIdentity};
        let destination = PathBuf::from("target/source.txt");
        let receipt = DirectoryCopyUndoReceipt {
            destination: destination.clone(),
            destination_parent_route: vec![SourceCleanupParentIdentity {
                path: PathBuf::from("target"),
                identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            }],
            root_identity: "1:0000000000000003".into(),
            entries: Vec::new(),
        };
        let terminal = |outcome| TerminalReport {
            outcome,
            final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt {
                item: ItemId(0), outcome: ItemOutcome::Completed, detail: None,
                created_folder_path: None, created_archive_path: None, move_receipt: None,
                replace_receipt: None, copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        };

        let mut accepted = OperationRuntime::default();
        let accepted_token = accepted.submit(request("source.txt")).unwrap();
        accepted.start_next().unwrap();
        accepted.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token: accepted_token, item: ItemId(0), receipt: receipt.clone() });
        accepted.commit_fact(ExecutorFact::Terminal { token: accepted_token, report: terminal(TerminalOutcome::Succeeded) }).unwrap();
        let (_, entry) = accepted.reserve_undo().expect("exact singleton success promotes the directory proof");
        assert!(matches!(entry, UndoEntry::DirectoryCopy(candidate) if candidate == receipt));

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let candidate = || ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: receipt.clone() };
        runtime.commit_fact(candidate());
        runtime.commit_fact(candidate());
        let mut mismatch = receipt.clone();
        mismatch.root_identity = "1:0000000000000004".into();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: mismatch });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(TerminalOutcome::Succeeded) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "conflicting duplicate evidence permanently invalidates this attempt");
        runtime.commit_fact(candidate());
        assert!(runtime.reserve_undo().is_none(), "late proof after accepted terminal is inert");

        for outcome in [TerminalOutcome::Partial, TerminalOutcome::Unknown, TerminalOutcome::Cancelled, TerminalOutcome::Failed] {
            let mut runtime = OperationRuntime::default();
            let token = runtime.submit(request("source.txt")).unwrap();
            runtime.start_next().unwrap();
            runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: receipt.clone() });
            runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(outcome) }).unwrap();
            assert!(runtime.reserve_undo().is_none(), "{outcome:?} cannot admit directory Copy Undo");
        }
    }

    #[test]
    fn directory_copy_group_undo_promotes_only_the_exact_completed_request_order() {
        use super::super::model::{DirectoryCopyUndoReceipt, SourceCleanupParentIdentity};
        let parent = PathBuf::from("target");
        let proof = |name: &str, id: u64| DirectoryCopyUndoReceipt {
            destination: parent.join(name),
            destination_parent_route: vec![SourceCleanupParentIdentity {
                path: parent.clone(),
                identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            }],
            root_identity: format!("1:{id:016x}"),
            entries: Vec::new(),
        };
        let receipts = vec![proof("a", 3), proof("b", 4)];
        let terminal = |outcome, completed: usize| TerminalReport {
            outcome,
            final_counters: ProgressCounters::default(),
            receipts: (0..completed).map(|index| ItemReceipt {
                item: ItemId(index as u32), outcome: ItemOutcome::Completed, detail: None,
                created_folder_path: None, created_archive_path: None, move_receipt: None,
                replace_receipt: None, copy_undo: None,
            }).collect(),
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        };
        let mut req = request("a");
        req.sources.push(PathBuf::from("b"));
        req.destination = Some(parent);
        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(req).unwrap();
        runtime.start_next().unwrap();
        for (index, receipt) in receipts.iter().cloned().enumerate() {
            runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(index as u32), receipt });
        }
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(TerminalOutcome::Succeeded, 2) }).unwrap();
        let (reservation, entry) = runtime.reserve_undo().expect("whole successful directory request becomes one Undo");
        assert!(matches!(entry, UndoEntry::DirectoryCopyGroup(actual) if actual == receipts));
        assert!(runtime.finish_directory_copy_group_undo(reservation, 1));
        let (reservation, entry) = runtime.reserve_undo().expect("one refusal retains the precise prefix for retry");
        assert!(matches!(entry, UndoEntry::DirectoryCopyGroup(actual) if actual == receipts[..1]));
        assert!(runtime.finish_directory_copy_group_undo(reservation, 1));
        assert!(runtime.undo_ledger.is_empty());

        for (terminal_outcome, supplied, candidate_count) in [
            (TerminalOutcome::Succeeded, 2, 1),
            (TerminalOutcome::Partial, 2, 2),
            (TerminalOutcome::Succeeded, 1, 2),
        ] {
            let mut req = request("a"); req.sources.push(PathBuf::from("b")); req.destination = Some(PathBuf::from("target"));
            let mut runtime = OperationRuntime::default(); let token = runtime.submit(req).unwrap(); runtime.start_next().unwrap();
            for (index, receipt) in receipts.iter().take(candidate_count).cloned().enumerate() {
                runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(index as u32), receipt });
            }
            runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(terminal_outcome, supplied) }).unwrap();
            assert!(runtime.reserve_undo().is_none(), "incomplete evidence and non-success terminals do not group");
        }

        let mut req = request("a"); req.sources.push(PathBuf::from("b")); req.destination = Some(PathBuf::from("target"));
        let mut runtime = OperationRuntime::default(); let token = runtime.submit(req).unwrap(); runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: receipts[0].clone() });
        let mut conflict = receipts[0].clone(); conflict.root_identity.push_str("-conflict");
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: conflict });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(1), receipt: receipts[1].clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(TerminalOutcome::Succeeded, 2) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "conflicting duplicate candidate invalidates the exact Completed group");
    }

    #[test]
    fn partial_directory_copy_group_uses_only_exact_completed_items() {
        use super::super::model::{DirectoryCopyUndoReceipt, SourceCleanupParentIdentity};
        let parent = PathBuf::from("target");
        let proof = |name: &str, id: u64| DirectoryCopyUndoReceipt {
            destination: parent.join(name),
            destination_parent_route: vec![SourceCleanupParentIdentity { path: parent.clone(), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2) }],
            root_identity: format!("1:{id:016x}"), entries: Vec::new(),
        };
        let proofs = [proof("a", 3), proof("c", 5)];
        let report = |outcome| TerminalReport {
            outcome, final_counters: ProgressCounters::default(),
            receipts: vec![
                ItemReceipt { item: ItemId(0), outcome: ItemOutcome::Completed, detail: None, created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None },
                ItemReceipt { item: ItemId(1), outcome: ItemOutcome::Failed, detail: None, created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None },
                ItemReceipt { item: ItemId(2), outcome: ItemOutcome::Completed, detail: None, created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None },
            ], retry: RetryPlan::RequiresReinspection, change_hints: Vec::new(),
        };
        let make_runtime = || {
            let mut request = request("a");
            request.sources = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
            request.destination = Some(parent.clone());
            let mut runtime = OperationRuntime::default();
            let token = runtime.submit(request).unwrap();
            runtime.start_next().unwrap();
            (runtime, token)
        };
        let (mut runtime, token) = make_runtime();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(2), receipt: proofs[1].clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: report(TerminalOutcome::Partial) }).unwrap();
        let (_, entry) = runtime.reserve_undo().expect("exact completed subset is undoable");
        assert!(matches!(entry, UndoEntry::DirectoryCopyGroup(actual) if actual == proofs));

        for evidence in [0, 1] {
            let (mut runtime, token) = make_runtime();
            if evidence != 0 {
                runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
                runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(2), receipt: proofs[1].clone() });
            }
            let terminal = report(if evidence == 1 { TerminalOutcome::Unknown } else { TerminalOutcome::Partial });
            runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal }).unwrap();
            assert!(runtime.reserve_undo().is_none(), "missing proof and Unknown reject the group");
        }

        let (mut runtime, token) = make_runtime();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(2), receipt: proofs[1].clone() });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(1), receipt: proof("b", 4) });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(1), receipt: proof("b", 6) });
        assert!(!runtime.directory_copy_undo_candidates.contains_key(&(token, ItemId(1))), "conflicting B proofs are removed from the candidate map");
        assert!(runtime.invalid_directory_copy_undo_candidates.contains(&(token, ItemId(1))), "conflicting B evidence leaves its invalid marker");
        runtime.commit_fact(ExecutorFact::Terminal { token, report: report(TerminalOutcome::Partial) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "an invalid noncompleted B proof rejects otherwise valid A/C evidence");

        let (mut runtime, token) = make_runtime();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(2), receipt: proofs[1].clone() });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(1), receipt: proof("b", 4) });
        assert!(runtime.directory_copy_undo_candidates.contains_key(&(token, ItemId(1))), "the valid extra B proof remains in the candidate map");
        runtime.commit_fact(ExecutorFact::Terminal { token, report: report(TerminalOutcome::Partial) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "one noncompleted B candidate rejects otherwise valid A/C evidence");

        for malformed in ["reordered", "duplicate", "out_of_range"] {
            let (mut runtime, token) = make_runtime();
            runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
            runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(2), receipt: proofs[1].clone() });
            let mut terminal = report(TerminalOutcome::Partial);
            match malformed {
                "reordered" => terminal.receipts.swap(0, 1),
                "duplicate" => terminal.receipts[2].item = ItemId(0),
                "out_of_range" => terminal.receipts[1].item = ItemId(9),
                _ => unreachable!(),
            }
            runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal }).unwrap();
            assert!(runtime.reserve_undo().is_none(), "malformed {malformed} terminal membership is rejected");
        }

        let (mut runtime, token) = make_runtime();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        let mut conflict = proofs[0].clone(); conflict.root_identity.push_str("-conflict");
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: conflict });
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(2), receipt: proofs[1].clone() });
        let accepted = runtime.commit_fact(ExecutorFact::Terminal { token, report: report(TerminalOutcome::Partial) }).unwrap();
        assert!(runtime.commit_fact(ExecutorFact::Terminal { token, report: report(TerminalOutcome::Partial) }).is_none(), "duplicate terminal is inert");
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        assert!(runtime.reserve_undo().is_none(), "conflicting proof rejects admission and late evidence cannot recreate it");
        let _ = accepted;
    }

    #[test]
    fn directory_copy_undo_rejects_stale_candidate_after_same_path_is_reused() {
        use super::super::model::{DirectoryCopyUndoReceipt, SourceCleanupParentIdentity};
        let receipt = DirectoryCopyUndoReceipt {
            destination: PathBuf::from("target/reused"),
            destination_parent_route: vec![SourceCleanupParentIdentity { path: PathBuf::from("target"), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2) }],
            root_identity: "1:0000000000000003".into(), entries: Vec::new(),
        };
        let report = |outcome| TerminalReport {
            outcome, final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt { item: ItemId(0), outcome: ItemOutcome::Completed, detail: None,
                created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None }],
            retry: RetryPlan::NotAllowed, change_hints: Vec::new(),
        };
        let mut runtime = OperationRuntime::default();
        let old = runtime.submit(request("reused")).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token: old, item: ItemId(0), receipt: receipt.clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token: old, report: report(TerminalOutcome::Partial) }).unwrap();
        let current = runtime.submit(request("reused")).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate { token: old, item: ItemId(0), receipt });
        runtime.commit_fact(ExecutorFact::Terminal { token: current, report: report(TerminalOutcome::Succeeded) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "old-attempt proof for a reused path cannot authorize the new exact attempt");
    }

    #[test]
    fn cross_volume_move_undo_requires_single_exact_succeeded_item() {
        use super::super::model::{CrossVolumeMoveUndoProof, MoveIdentityGuard, MoveUndoReceipt, SourceCleanupLeafSignature, SourceCleanupParentIdentity};
        let source = PathBuf::from(r"C:\source\item.bin");
        let destination = PathBuf::from(r"E:\destination\item.bin");
        let source_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 13);
        let destination_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 21);
        let source_parent = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 11);
        let destination_parent = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 12);
        let signature = SourceCleanupLeafSignature { number_of_links: 1, change_time: 10 };
        let receipt = MoveUndoReceipt {
            source: source.clone(), destination: destination.clone(), object_identity: destination_identity,
            object_kind: super::super::model::MoveUndoObjectKind::RegularFile,
            source_parent_route: vec![
                SourceCleanupParentIdentity { path: PathBuf::from(r"C:\"), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 1) },
                SourceCleanupParentIdentity { path: source.parent().unwrap().to_path_buf(), identity: source_parent },
            ],
            destination_parent_route: vec![
                SourceCleanupParentIdentity { path: PathBuf::from(r"E:\"), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 2) },
                SourceCleanupParentIdentity { path: destination.parent().unwrap().to_path_buf(), identity: destination_parent.clone() },
            ],
            cross_volume_file: Some(CrossVolumeMoveUndoProof {
                original_source_identity: source_identity.clone(), original_source_signature: signature,
                destination_signature: signature, content_sha256: "a".repeat(64),
                directory_manifest: None,
            }),
        };
        let make_report = |outcome| TerminalReport {
            outcome, final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt {
                item: ItemId(0), outcome: ItemOutcome::Completed, detail: None,
                created_folder_path: None, created_archive_path: None,
                move_receipt: Some(super::super::model::MoveReceipt {
                    source: source.clone(), destination: destination.clone(), destination_published: true, source_removed: true,
                }), replace_receipt: None, copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed, change_hints: vec![],
        };
        let make_request = || {
            let mut request = request(source.to_string_lossy().as_ref());
            request.kind = OperationKind::Move;
            request.destination = Some(destination.parent().unwrap().to_path_buf());
            request.move_identity_guard = Some(MoveIdentityGuard {

                source_identities: vec![source_identity.clone()],
                destination_identity: destination_parent.clone(),
                });
            request
        };
        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: receipt.clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: make_report(TerminalOutcome::Succeeded) }).unwrap();
        let (_, admitted) = runtime.reserve_undo().expect("exact completed direct cross-volume Move receives process-local Undo");
        assert!(matches!(admitted, UndoEntry::Move(receipts) if receipts == vec![receipt.clone()]));

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: receipt.clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: make_report(TerminalOutcome::Partial) }).unwrap();
        let (_, admitted) = runtime.reserve_undo().expect("Partial terminal admits its exact Completed cross-volume receipt");
        assert!(matches!(admitted, UndoEntry::Move(receipts) if receipts == vec![receipt.clone()]));
    }

    #[test]
    fn cross_volume_directory_move_undo_admits_exact_ordered_batch_and_partial_subset() {
        use super::super::{
            journal::{JournalDirectoryMove, JournalDirectoryRoot, JournalPath},
            model::{CrossVolumeMoveUndoProof, MoveIdentityGuard, MoveUndoReceipt, MoveUndoObjectKind, SourceCleanupLeafSignature, SourceCleanupParentIdentity},
        };

        let sources = (0..3)
            .map(|index| PathBuf::from(format!(r"C:\source\folder{index}")))
            .collect::<Vec<_>>();
        let destination_parent = PathBuf::from(r"E:\destination");
        let source_parent_id = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 11);
        let destination_parent_id = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 12);
        let identity_text = |identity: &crate::filesystem_identity::FilesystemIdentity| {
            let (volume, index) = identity.windows_parts().unwrap();
            format!("{volume}:{:08x}{:08x}", (index >> 32) as u32, index as u32)
        };
        let candidates = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let destination = destination_parent.join(source.file_name().unwrap());
                let source_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 20 + index as u64);
                let destination_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 30 + index as u64);
                let manifest = JournalDirectoryMove {
                    item: index as u32,
                    source: JournalPath::from_path(source),
                    destination: JournalPath::from_path(&destination),
                    children: Vec::new(),
                    pending: None,
                    root: Some(JournalDirectoryRoot {
                        source_identity: identity_text(&source_identity),
                        destination_identity: identity_text(&destination_identity),
                        pending: None,
                        disposition_recorded: true,
                    }),
                    cleanup_complete: true,
                    replace: None,
                };
                MoveUndoReceipt {
                    source: source.clone(),
                    destination: destination.clone(),
                    object_identity: destination_identity,
                    object_kind: MoveUndoObjectKind::Directory,
                    source_parent_route: vec![
                        SourceCleanupParentIdentity {
                            path: PathBuf::from(r"C:\"),
                            identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 1),
                        },
                        SourceCleanupParentIdentity { path: source_parent(), identity: source_parent_id.clone() },
                    ],
                    destination_parent_route: vec![
                        SourceCleanupParentIdentity {
                            path: PathBuf::from(r"E:\"),
                            identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 2),
                        },
                        SourceCleanupParentIdentity { path: destination_parent.clone(), identity: destination_parent_id.clone() },
                    ],
                    cross_volume_file: Some(CrossVolumeMoveUndoProof {
                        original_source_identity: source_identity,
                        original_source_signature: SourceCleanupLeafSignature { number_of_links: 1, change_time: 100 + index as i64 },
                        destination_signature: SourceCleanupLeafSignature { number_of_links: 1, change_time: 200 + index as i64 },
                        content_sha256: format!("{:064x}", index + 1),
                        directory_manifest: Some(manifest),
                    }),
                }
            })
            .collect::<Vec<_>>();

        fn source_parent() -> PathBuf {
            PathBuf::from(r"C:\source")
        }

        let mut operation = request(sources[0].to_string_lossy().as_ref());
        operation.kind = OperationKind::Move;
        operation.sources = sources.clone();
        operation.destination = Some(destination_parent.clone());
        operation.move_identity_guard = Some(MoveIdentityGuard {

            source_identities: candidates.iter().map(|candidate| candidate.cross_volume_file.as_ref().unwrap().original_source_identity.clone()).collect(),
            destination_identity: destination_parent_id,
            });

        for (items, outcome, expected) in [
            (vec![(0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed)], TerminalOutcome::Succeeded, vec![0, 1, 2]),
            (vec![(0, ItemOutcome::Completed), (1, ItemOutcome::Failed), (2, ItemOutcome::Completed)], TerminalOutcome::Partial, vec![0, 2]),
        ] {
            let mut runtime = OperationRuntime::default();
            let token = runtime.submit(operation.clone()).unwrap();
            runtime.start_next().unwrap();
            for (index, candidate) in candidates.iter().enumerate() {
                runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(index as u32), receipt: candidate.clone() });
            }
            let report = TerminalReport {
                outcome,
                final_counters: ProgressCounters::default(),
                receipts: items.iter().map(|(index, item_outcome)| ItemReceipt {
                    item: ItemId(*index as u32),
                    outcome: *item_outcome,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: Some(super::super::model::MoveReceipt {
                        source: candidates[*index].source.clone(),
                        destination: candidates[*index].destination.clone(),
                        destination_published: true,
                        source_removed: true,
                    }),
                    replace_receipt: None,
                    copy_undo: None,
                }).collect(),
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            };
            runtime.commit_fact(ExecutorFact::Terminal { token, report }).unwrap();
            let (_, entry) = runtime.reserve_undo().expect("exact completed directory candidates admit as one group");
            assert!(matches!(entry, UndoEntry::Move(receipts)
                if receipts.iter().map(|receipt| sources.iter().position(|source| source == &receipt.source).unwrap()).collect::<Vec<_>>() == expected));
        }
    }

    #[test]
    fn cross_volume_move_undo_accepts_exact_batches_and_partial_completed_subset() {
        use super::super::model::{CrossVolumeMoveUndoProof, MoveIdentityGuard, MoveUndoReceipt, SourceCleanupLeafSignature, SourceCleanupParentIdentity};
        let sources = [PathBuf::from(r"C:\source\a.bin"), PathBuf::from(r"C:\source\b.bin"), PathBuf::from(r"C:\source\c.bin")];
        let destination_parent = PathBuf::from(r"E:\destination");
        let destination_parent_id = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 12);
        let signature = SourceCleanupLeafSignature { number_of_links: 1, change_time: 7 };
        let proofs = sources.iter().enumerate().map(|(index, source)| {
            let source_id = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 20 + index as u64);
            let output_id = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 30 + index as u64);
            let destination = destination_parent.join(source.file_name().unwrap());
            MoveUndoReceipt {
                source: source.clone(), destination: destination.clone(), object_identity: output_id,
                object_kind: super::super::model::MoveUndoObjectKind::RegularFile,
                source_parent_route: vec![
                    SourceCleanupParentIdentity { path: PathBuf::from(r"C:\"), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 1) },
                    SourceCleanupParentIdentity { path: source.parent().unwrap().to_path_buf(), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 11) },
                ],
                destination_parent_route: vec![
                    SourceCleanupParentIdentity { path: PathBuf::from(r"E:\"), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(8, 2) },
                    SourceCleanupParentIdentity { path: destination_parent.clone(), identity: destination_parent_id.clone() },
                ],
                cross_volume_file: Some(CrossVolumeMoveUndoProof {
                    original_source_identity: source_id, original_source_signature: signature,
                    destination_signature: signature, content_sha256: format!("{:064x}", index + 1),
                    directory_manifest: None,
                }),
            }
        }).collect::<Vec<_>>();
        let make_request = || {
            let mut request = request(sources[0].to_string_lossy().as_ref());
            request.kind = OperationKind::Move;
            request.sources = sources.to_vec();
            request.destination = Some(destination_parent.clone());
            request.move_identity_guard = Some(MoveIdentityGuard {

                source_identities: proofs.iter().map(|proof| proof.cross_volume_file.as_ref().unwrap().original_source_identity.clone()).collect(),
                destination_identity: destination_parent_id.clone(),
                });
            request
        };
        let terminal = |items: &[(usize, ItemOutcome)]| TerminalReport {
            outcome: if items.iter().all(|(_, outcome)| *outcome == ItemOutcome::Completed) { TerminalOutcome::Succeeded } else { TerminalOutcome::Partial },
            final_counters: ProgressCounters::default(),
            receipts: items.iter().map(|(index, outcome)| ItemReceipt {
                item: ItemId(*index as u32), outcome: *outcome, detail: None, created_folder_path: None,
                created_archive_path: None,
                move_receipt: Some(super::super::model::MoveReceipt {
                    source: proofs[*index].source.clone(), destination: proofs[*index].destination.clone(),
                    destination_published: true, source_removed: true,
                }), replace_receipt: None, copy_undo: None,
            }).collect(), retry: RetryPlan::NotAllowed, change_hints: vec![],
        };
        for (items, expected) in [
            (vec![(0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed)], vec![0, 1, 2]),
            (vec![(0, ItemOutcome::Completed), (1, ItemOutcome::Failed), (2, ItemOutcome::Completed)], vec![0, 2]),
        ] {
            let mut runtime = OperationRuntime::default();
            let token = runtime.submit(make_request()).unwrap();
            runtime.start_next().unwrap();
            for (index, proof) in proofs.iter().enumerate() {
                runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(index as u32), receipt: proof.clone() });
            }
            runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(&items) }).unwrap();
            let (_, entry) = runtime.reserve_undo().expect("exact completed cross-volume subset is admitted");
            assert!(matches!(entry, UndoEntry::Move(receipts) if receipts.iter().map(|receipt| sources.iter().position(|source| source == &receipt.source).unwrap()).collect::<Vec<_>>() == expected));
        }

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        for (index, proof) in proofs.iter().enumerate() {
            runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(index as u32), receipt: proof.clone() });
        }
        let mut duplicate = terminal(&[(0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed)]);
        duplicate.receipts.push(duplicate.receipts[1].clone());
        runtime.commit_fact(ExecutorFact::Terminal { token, report: duplicate }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "duplicate item receipt rejects the cross-volume batch");

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(2), receipt: proofs[2].clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(&[(0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed)]) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "missing Completed candidate rejects the whole cross-volume group");

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        let mut same_mode = proofs[1].clone();
        let source_identity = same_mode.cross_volume_file.take().unwrap().original_source_identity;
        same_mode.object_identity = source_identity;
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(1), receipt: same_mode });
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(2), receipt: proofs[2].clone() });
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(&[
            (0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed),
        ]) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "mixed same-volume and cross-volume candidate proof rejects the group");

        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(0), receipt: proofs[0].clone() });
        for (index, proof) in proofs.iter().enumerate().skip(1) {
            runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token, item: ItemId(index as u32), receipt: proof.clone() });
        }
        runtime.commit_fact(ExecutorFact::Terminal { token, report: terminal(&[
            (0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed),
        ]) }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "duplicate candidate invalidates the whole cross-volume group");

        let mut runtime = OperationRuntime::default();
        let expired = runtime.submit(make_request()).unwrap();
        runtime.start_next().unwrap();
        let mut unknown = terminal(&[
            (0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed),
        ]);
        unknown.outcome = TerminalOutcome::Unknown;
        runtime.commit_fact(ExecutorFact::Terminal { token: expired, report: unknown }).unwrap();
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token: expired, item: ItemId(0), receipt: proofs[0].clone() });
        assert!(runtime.reserve_undo().is_none(), "late exact cross-volume candidate after Unknown cannot authorize Undo");

        let successor = runtime.submit(make_request()).unwrap();
        assert_ne!(expired, successor, "same-path successor receives a new attempt token");
        runtime.start_next().expect("same-path successor starts after Unknown terminal");
        runtime.commit_fact(ExecutorFact::MoveUndoCandidate { token: expired, item: ItemId(0), receipt: proofs[0].clone() });
        runtime.commit_fact(ExecutorFact::Terminal {
            token: successor,
            report: terminal(&[
                (0, ItemOutcome::Completed), (1, ItemOutcome::Completed), (2, ItemOutcome::Completed),
            ]),
        }).unwrap();
        assert!(runtime.reserve_undo().is_none(), "prior-token cross-volume proof cannot authorize a running same-path successor");
    }

    #[test]
    fn grouped_move_undo_settlement_consumes_only_confirmed_suffix_and_releases_lane() {
        use super::super::model::{MoveUndoReceipt, SourceCleanupParentIdentity};
        let identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 11);
        let receipt = MoveUndoReceipt {
            source: PathBuf::from(r"C:\source\a"),
            destination: PathBuf::from(r"C:\destination\a"),
            object_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(7, 12),
            object_kind: super::super::model::MoveUndoObjectKind::Directory,
            source_parent_route: vec![SourceCleanupParentIdentity { path: PathBuf::from(r"C:\source"), identity: identity.clone() }],
            destination_parent_route: vec![SourceCleanupParentIdentity { path: PathBuf::from(r"C:\destination"), identity }],
            cross_volume_file: None,
        };
        let mut runtime = OperationRuntime::default();
        runtime.undo_ledger.push(UndoEntry::Move(vec![receipt.clone(), receipt.clone(), receipt.clone()]));
        let (reservation, _) = runtime.reserve_undo().unwrap();
        assert!(runtime.finish_move_undo(reservation, 1));
        let (reserved, entries) = runtime.undo_snapshot_for_test();
        assert!(!reserved);
        assert!(matches!(entries.as_slice(), [UndoEntry::Move(receipts)] if receipts == &[receipt.clone(), receipt.clone()]));
        let (retry_reservation, _) = runtime.reserve_undo().unwrap();
        assert!(runtime.finish_move_undo(retry_reservation, 0));
        assert!(!runtime.undo_snapshot_for_test().0, "zero-success retry releases the serial lane");
        let (retry_reservation, _) = runtime.reserve_undo().unwrap();
        assert!(runtime.finish_move_undo(retry_reservation, 2));
        assert!(runtime.undo_snapshot_for_test().1.is_empty());
        assert!(!runtime.finish_move_undo(retry_reservation, 1), "stale completion cannot settle another reservation");
    }

    #[test]
    fn partial_copy_terminal_admits_only_completed_item_undo_receipts() {
        let mut runtime = OperationRuntime::default();
        let token = runtime.submit(request("source.txt")).unwrap();
        runtime.start_next().unwrap();
        let receipt = super::super::model::CopyUndoReceipt {
            destination: PathBuf::from("target/completed.txt"),
            parent_identity: "1:0000000000000002".into(),
            file_identity: "1:0000000000000003".into(),
            content_sha256: "0123456789abcdef".into(),
            leaf_signature: super::super::model::SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 42,
            },
        };
        let should_not_be_admitted = super::super::model::CopyUndoReceipt {
            destination: PathBuf::from("target/failed.txt"),
            ..receipt.clone()
        };
        runtime
            .commit_fact(ExecutorFact::Terminal {
                token,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    final_counters: ProgressCounters::default(),
                    receipts: vec![
                        ItemReceipt {
                            item: ItemId(0),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: Some(receipt.clone()),
                        },
                        ItemReceipt {
                            item: ItemId(1),
                            outcome: ItemOutcome::Failed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: Some(should_not_be_admitted.clone()),
                        },
                    ],
                    retry: RetryPlan::RequiresReinspection,
                    change_hints: vec![],
                },
            })
            .expect("current partial terminal should commit");

        let (_, admitted) = runtime
            .reserve_undo()
            .expect("completed prefix output should be undoable");
        assert!(matches!(admitted, UndoEntry::Copy(found) if found == receipt));
        assert!(
            runtime.reserve_undo().is_none(),
            "failed-item proof must be ignored"
        );
    }

    #[test]
    fn partial_file_copy_replace_group_admits_only_exact_completed_replace_output() {
        use super::super::executor::OrdinaryCopySourceKind;
        use super::super::model::{CopyUndoReceipt, ReplaceReceipt, SourceCleanupLeafSignature};

        let sources = [PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        let destination = PathBuf::from("target");
        let output = destination.join("a.txt");
        let proof = CopyUndoReceipt {
            destination: output.clone(),
            parent_identity: "1:0000000000000002".into(),
            file_identity: "1:0000000000000003".into(),
            content_sha256: "0123456789abcdef".into(),
            leaf_signature: SourceCleanupLeafSignature { number_of_links: 1, change_time: 42 },
        };
        let replace = ReplaceReceipt {
            source: sources[0].clone(),
            destination: output.clone(),
            displaced_backup: Some(destination.join(".a.txt.backup")),
            destination_published: true,
            displaced_backup_removed: true,
        };
        let make_runtime = || {
            let mut req = request("a.txt");
            req.sources = sources.to_vec();
            req.destination = Some(destination.clone());
            let mut runtime = OperationRuntime::default();
            let token = runtime.submit(req).unwrap();
            runtime.start_next().unwrap();
            for item in [ItemId(0), ItemId(1)] {
                runtime.commit_fact(ExecutorFact::OrdinaryCopySourceKind {
                    token, item, kind: OrdinaryCopySourceKind::File,
                });
            }
            (runtime, token)
        };
        let report = |replace_receipt: Option<ReplaceReceipt>, failed_copy_undo: Option<CopyUndoReceipt>| TerminalReport {
            outcome: TerminalOutcome::Partial,
            final_counters: ProgressCounters::default(),
            receipts: vec![
                ItemReceipt {
                    item: ItemId(0), outcome: ItemOutcome::Completed, detail: None,
                    created_folder_path: None, created_archive_path: None, move_receipt: None,
                    replace_receipt, copy_undo: Some(proof.clone()),
                },
                ItemReceipt {
                    item: ItemId(1), outcome: ItemOutcome::Failed, detail: None,
                    created_folder_path: None, created_archive_path: None, move_receipt: None,
                    replace_receipt: None, copy_undo: failed_copy_undo,
                },
            ],
            retry: RetryPlan::RequiresReinspection,
            change_hints: vec![],
        };

        let (mut runtime, token) = make_runtime();
        let accepted = runtime.commit_fact(ExecutorFact::Terminal {
            token, report: report(Some(replace.clone()), None),
        }).expect("first-attempt Partial terminal should commit");
        assert!(matches!(accepted.event, ServiceEvent::TerminalCommitted { report: ref terminal, .. }
            if terminal.receipts.iter().map(|receipt| receipt.item).collect::<Vec<_>>() == [ItemId(0), ItemId(1)]));
        let (reservation, entry) = runtime.reserve_undo().expect("completed Replace output should be admitted");
        assert!(matches!(entry, UndoEntry::MixedCopyGroup(items)
            if items == vec![super::super::model::CopyUndoItem::File(proof.clone())]));
        assert!(runtime.finish_mixed_copy_group_undo(reservation, 1), "consume the one completed output");
        assert!(runtime.reserve_undo().is_none(), "consumed group cannot be reserved again");
        assert!(runtime.commit_fact(ExecutorFact::Terminal {
            token, report: report(Some(replace.clone()), None),
        }).is_none(), "duplicate terminal cannot recreate the group");
        runtime.commit_fact(ExecutorFact::Progress {
            token, update: ProgressUpdate { sequence: 99, counters: ProgressCounters::default(), current_item: None },
        });
        assert!(runtime.reserve_undo().is_none(), "late progress cannot recreate consumed authority");

        let (mut runtime, token) = make_runtime();
        let mut malformed_cleanup = replace;
        malformed_cleanup.displaced_backup_removed = false;
        runtime.commit_fact(ExecutorFact::Terminal {
            token, report: report(Some(malformed_cleanup), None),
        }).expect("malformed-proof terminal itself still commits");
        assert!(runtime.reserve_undo().is_none(), "Replace without confirmed backup cleanup is not admitted");

        let (mut runtime, token) = make_runtime();
        runtime.commit_fact(ExecutorFact::Terminal {
            token, report: report(Some(ReplaceReceipt {
                source: sources[0].clone(), destination: output,
                displaced_backup: Some(destination.join(".a.txt.backup")),
                destination_published: true, displaced_backup_removed: true,
            }), Some(CopyUndoReceipt {
                destination: destination.join("b.txt"), ..proof.clone()
            })),
        }).expect("failed-item evidence terminal itself still commits");
        assert!(runtime.reserve_undo().is_none(), "failed-item Undo proof invalidates Replace group admission");
    }

    #[test]
    fn non_success_or_partial_copy_terminals_do_not_admit_copy_undo() {
        for outcome in [
            TerminalOutcome::Cancelled,
            TerminalOutcome::Failed,
            TerminalOutcome::Unknown,
        ] {
            let mut runtime = OperationRuntime::default();
            let token = runtime.submit(request("source.txt")).unwrap();
            runtime.start_next().unwrap();
            runtime
                .commit_fact(ExecutorFact::Terminal {
                    token,
                    report: TerminalReport {
                        outcome,
                        final_counters: ProgressCounters::default(),
                        receipts: vec![ItemReceipt {
                            item: ItemId(0),
                            outcome: ItemOutcome::Completed,
                            detail: None,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: Some(super::super::model::CopyUndoReceipt {
                                destination: PathBuf::from("target/source.txt"),
                                parent_identity: "1:0000000000000002".into(),
                                file_identity: "1:0000000000000003".into(),
                                content_sha256: "0123456789abcdef".into(),
                                leaf_signature: super::super::model::SourceCleanupLeafSignature {
                                    number_of_links: 1,
                                    change_time: 42,
                                },
                            }),
                        }],
                        retry: RetryPlan::RequiresReinspection,
                        change_hints: vec![],
                    },
                })
                .expect("current terminal should commit");
            assert!(
                runtime.reserve_undo().is_none(),
                "{outcome:?} must not admit Copy undo"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn real_partial_copy_batch_can_undo_only_its_published_completed_prefix() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("source.bin");
        let missing_source = root.path().join("missing.bin");
        let destination = root.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(&source, b"published copy bytes").unwrap();
        let output = destination.join("source.bin");
        let mut request = request("source.bin");
        request.sources = vec![source.clone(), missing_source.clone()];
        request.destination = Some(destination);

        let mut runtime = OperationRuntime::default();
        runtime.submit(request).unwrap();
        let start = runtime.start_next().expect("copy should start");
        let mut facts = Vec::new();
        super::super::executor::execute_prepare(
            start.command,
            &start.cancellation,
            &start.controls,
            |fact| facts.push(fact),
        );

        let mut accepted_terminal = None;
        for fact in facts {
            if let Some(commit) = runtime.commit_fact(fact) {
                accepted_terminal = Some(commit);
            }
        }
        let commit = accepted_terminal.expect("executor terminal should commit");
        let super::ServiceEvent::TerminalCommitted { report, .. } = commit.event;
        assert_eq!(report.outcome, TerminalOutcome::Partial);
        assert_eq!(report.receipts.len(), 2);
        assert_eq!(report.receipts[0].outcome, ItemOutcome::Completed);
        assert!(report.receipts[0].copy_undo.is_some());
        assert_eq!(report.receipts[1].outcome, ItemOutcome::Failed);
        assert!(report.receipts[1].copy_undo.is_none());

        let (reservation, undo) = runtime
            .reserve_undo()
            .expect("completed output should be undoable");
        let UndoEntry::Copy(receipt) = undo else {
            panic!("partial Copy should admit the completed output receipt");
        };
        assert_eq!(receipt.destination, output);
        assert!(super::super::executor::undo_copy(receipt).is_ok());
        assert!(runtime.finish_trash_undo(reservation, true));
        assert!(
            runtime.reserve_undo().is_none(),
            "failed suffix must not create undo"
        );
        assert!(!output.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"published copy bytes");
        assert!(!missing_source.exists());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "modifies this user account's Recycle Bin; invoke explicitly on a disposable fixture"]
    fn real_partial_trash_batch_undoes_only_the_completed_prefix() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };

        struct Cleanup {
            first: PathBuf,
            second: PathBuf,
            first_receipt: Arc<Mutex<Option<super::super::trash::TrashReceipt>>>,
        }

        fn restore_exact_recycle_path(path: &std::path::Path) -> bool {
            let matches = match ::trash::os_limited::list() {
                Ok(items) => items
                    .into_iter()
                    .filter(|item| item.original_path() == path)
                    .collect::<Vec<_>>(),
                Err(error) => {
                    eprintln!("Partial Trash cleanup could not inspect Recycle Bin: {error}");
                    return false;
                }
            };
            if let [item] = matches.as_slice() {
                return super::super::trash::restore_recovered_item_for_test(item.id.clone(), path)
                    .is_ok();
            }
            false
        }

        impl Drop for Cleanup {
            fn drop(&mut self) {
                if !self.first.exists() {
                    let restored_by_receipt = if let Some(receipt) = self
                        .first_receipt
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take()
                    {
                        match super::super::trash::restore(receipt) {
                            Ok(_) => true,
                            Err(error) => {
                                eprintln!(
                                    "Partial Trash cleanup could not restore first receipt: {error:?}"
                                );
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if !restored_by_receipt && !restore_exact_recycle_path(&self.first) {
                        eprintln!(
                            "Partial Trash cleanup could not safely recover first fixture {}; preserving it",
                            self.first.display()
                        );
                    }
                }
                if !self.second.exists() {
                    if !restore_exact_recycle_path(&self.second) {
                        eprintln!(
                            "Partial Trash cleanup could not safely recover second fixture {}; preserving it",
                            self.second.display()
                        );
                    }
                }
            }
        }

        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let first = root.path().join("partial-trash-completed.txt");
        let second = root.path().join("partial-trash-predelete-abort.txt");
        std::fs::write(&first, b"completed Trash prefix").unwrap();
        std::fs::write(&second, b"uncertain PreDeleteItem suffix").unwrap();
        let first_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &first,
            crate::directory::EntryKind::File,
        )
        .expect("first Trash fixture identity");
        let second_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &second,
            crate::directory::EntryKind::File,
        )
        .expect("second Trash fixture identity");
        let first_receipt = Arc::new(Mutex::new(None));
        let _cleanup = Cleanup {
            first: first.clone(),
            second: second.clone(),
            first_receipt: first_receipt.clone(),
        };

        let mut operation = request("partial-trash-completed.txt");
        operation.kind = OperationKind::Trash;
        operation.sources = vec![first.clone(), second.clone()];
        operation.destination = None;
        operation.trash_source_guards = Some(vec![
            super::super::model::TrashSourceGuard {
                identity: first_identity.clone(),
                reparse_candidate: false,
            },
            super::super::model::TrashSourceGuard {
                identity: second_identity.clone(),
                reparse_candidate: false,
            },
        ]);

        let abort_reached_pre_delete = Arc::new(AtomicBool::new(false));
        let abort_callback = abort_reached_pre_delete.clone();
        let first_for_adapter = first.clone();
        let receipt_for_adapter = first_receipt.clone();
        let mut runtime = OperationRuntime::default();
        runtime
            .submit(operation)
            .expect("Trash request is admitted");
        let start = runtime.start_next().expect("Trash request starts");
        let mut facts = Vec::new();
        super::super::executor::execute_prepare_with_trash_identity_adapter_for_test(
            start.command,
            &start.cancellation,
            &start.controls,
            |fact| {
                if let super::super::executor::ExecutorFact::TrashReceipt { receipt, .. } = &fact {
                    *receipt_for_adapter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(receipt.clone());
                }
                facts.push(fact);
            },
            move |path, accepted| {
                if path == first_for_adapter {
                    super::super::trash::move_to_trash_with_identity(path, accepted)
                } else {
                    super::super::trash::move_to_trash_aborting_in_pre_delete_for_test(
                        path,
                        abort_callback.clone(),
                    )
                }
            },
        );

        assert!(
            abort_reached_pre_delete.load(Ordering::Acquire),
            "second item must reach the real Shell PreDeleteItem callback"
        );
        assert!(
            !first.exists(),
            "completed prefix item is in the Recycle Bin"
        );
        assert_eq!(
            std::fs::read(&second).unwrap(),
            b"uncertain PreDeleteItem suffix"
        );
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &second,
                crate::directory::EntryKind::File,
            ),
            Some(second_identity),
            "the uncertain item remains at its original path"
        );

        let mut accepted_terminal = None;
        for fact in facts {
            if let Some(commit) = runtime.commit_fact(fact) {
                accepted_terminal = Some(commit);
            }
        }
        let commit = accepted_terminal.expect("executor terminal is committed by runtime");
        let super::ServiceEvent::TerminalCommitted { report, .. } = commit.event;
        assert_eq!(report.outcome, TerminalOutcome::Partial);
        assert_eq!(report.receipts.len(), 2);
        assert_eq!(report.receipts[0].outcome, ItemOutcome::Completed);
        assert_eq!(report.receipts[1].outcome, ItemOutcome::Unknown);
        assert!(report.receipts[1].copy_undo.is_none());

        let (reservation, undo) = runtime
            .reserve_undo()
            .expect("completed Trash prefix should be undoable");
        let UndoEntry::Trash(receipt) = undo else {
            panic!("Partial Trash must promote its exact opaque receipt");
        };
        assert_eq!(
            super::super::trash::restore(receipt).unwrap(),
            super::super::trash::RestoreDisposition::Completed
        );
        *first_receipt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        assert!(runtime.finish_trash_undo(reservation, true));
        assert!(
            runtime.reserve_undo().is_none(),
            "unknown suffix grants no undo"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"completed Trash prefix");
        assert_eq!(
            std::fs::read(&second).unwrap(),
            b"uncertain PreDeleteItem suffix"
        );
        assert_eq!(
            crate::directory::EntrySourceIdentity::from_path_no_follow(
                &first,
                crate::directory::EntryKind::File,
            ),
            Some(first_identity),
            "production restore returns the exact completed-prefix identity"
        );
    }
}
