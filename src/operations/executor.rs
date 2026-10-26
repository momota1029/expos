//! Windows E2 copy primitive.  This is deliberately detached from GPUI and jobs.
mod virtual_copy;

use std::{
    cell::RefCell,
    ffi::OsStr,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::Receiver,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// One application-owned slot for inert handles left by a failed directory
/// Copy Undo cleanup. It is shared only with that cleanup worker.
#[derive(Clone)]
pub(crate) struct DirectoryCopyUndoCustodySlot {
    inner: std::sync::Arc<std::sync::Mutex<Option<DirectoryCopyUndoCustody>>>,
    #[cfg(test)]
    cleanup_close_fault: std::sync::Arc<std::sync::atomic::AtomicU8>,
    // 並行 fault test 間で raw handle を取り違えないよう、この Undo invocation に結び付ける。
    #[cfg(test)]
    injected_close_handle: std::sync::Arc<std::sync::atomic::AtomicIsize>,
    #[cfg(test)]
    undo_worker_barrier: std::sync::Arc<std::sync::Mutex<Option<(
        async_channel::Sender<()>,
        async_channel::Receiver<()>,
    )>>>,
}

#[derive(Default)]
pub(crate) struct DirectoryCopyUndoCustody {
    #[cfg(windows)]
    _owned_handles: Vec<std::os::windows::io::OwnedHandle>,
    #[cfg(windows)]
    _opaque_uncertain_handles: Vec<usize>,
}

impl DirectoryCopyUndoCustodySlot {
    pub(crate) fn is_occupied(&self) -> bool {
        self.inner.lock().unwrap_or_else(|poison| poison.into_inner()).is_some()
    }

    pub(crate) fn install(&self, custody: DirectoryCopyUndoCustody) {
        let mut slot = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        assert!(slot.is_none(), "directory Copy Undo custody slot is single assignment");
        *slot = Some(custody);
    }

    #[cfg(test)]
    pub(crate) fn inject_cleanup_close_fault_for_test(&self, root: bool) {
        self.cleanup_close_fault.store(if root { 1 } else { 2 }, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn arm_undo_worker_barrier_for_test(
        &self,
        started: async_channel::Sender<()>,
        release: async_channel::Receiver<()>,
    ) {
        *self.undo_worker_barrier.lock().unwrap_or_else(|poison| poison.into_inner()) =
            Some((started, release));
    }

    #[cfg(test)]
    pub(crate) fn take_undo_worker_barrier_for_test(
        &self,
    ) -> Option<(async_channel::Sender<()>, async_channel::Receiver<()>)> {
        self.undo_worker_barrier.lock().unwrap_or_else(|poison| poison.into_inner()).take()
    }

    #[cfg(test)]
    fn take_cleanup_close_fault_for_test(
        &self,
    ) -> Option<(bool, std::sync::Arc<std::sync::atomic::AtomicIsize>)> {
        match self.cleanup_close_fault.swap(0, Ordering::AcqRel) {
            1 => Some((true, self.injected_close_handle.clone())),
            2 => Some((false, self.injected_close_handle.clone())),
            _ => None,
        }
    }
}

impl Default for DirectoryCopyUndoCustodySlot {
    fn default() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            cleanup_close_fault: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            #[cfg(test)]
            injected_close_handle: std::sync::Arc::new(std::sync::atomic::AtomicIsize::new(0)),
            #[cfg(test)]
            undo_worker_barrier: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

use super::model::{
    AttemptToken, ExecutorCommand, FsChangeHint, ItemId, ItemOutcome, ItemReceipt, MoveReceipt,
    OperationKind, ProgressCounters, ProgressUpdate, ReplaceReceipt, RetryPlan, TerminalOutcome,
    TerminalReport,
};

mod create_file;
mod create_folder;
mod create_shortcut;
#[cfg(windows)]
mod seven_zip_extract;
mod zip_create;
mod zip_extract;

const CHUNK: usize = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// 実機確認で高速なローカル SSD の途中 progress を撮るためだけの opt-in delay。
/// 通常は環境変数が無いので `None` の一回判定だけで、I/O の順序を変えない。
const COPY_THROTTLE_MAX_MS: u64 = 100;
const COPY_THROTTLE_POLL: Duration = Duration::from_millis(5);

#[cfg(target_os = "windows")]
fn copy_throttle() -> Option<Duration> {
    static THROTTLE: OnceLock<Option<Duration>> = OnceLock::new();
    *THROTTLE.get_or_init(|| {
        copy_throttle_from(std::env::var_os("GPUI_FILER_COPY_THROTTLE_MS").as_deref())
    })
}

fn copy_throttle_from(value: Option<&OsStr>) -> Option<Duration> {
    let milliseconds = value?.to_str()?.parse::<u64>().ok()?;
    (milliseconds > 0).then(|| Duration::from_millis(milliseconds.min(COPY_THROTTLE_MAX_MS)))
}

/// 一括 sleep にせず、Cancel を短い間隔で受け取る。呼び出し元は owned staging
/// output を cleanup して既存の Cancelled terminal 経路へ戻す。
#[cfg(target_os = "windows")]
fn wait_for_copy_throttle(cancelled: &AtomicBool) -> Result<(), CopyError> {
    let Some(delay) = copy_throttle() else {
        return Ok(());
    };
    let deadline = Instant::now() + delay;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        std::thread::sleep(remaining.min(COPY_THROTTLE_POLL));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyError {
    Cancelled,
    Unsupported,
    /// A request-time filesystem identity could not be proven at mutation time.
    /// This is never eligible for cross-volume or elevation fallback.
    GuardRejected(String),
    /// Atomic no-replace publication found that another object owns the name.
    DestinationExists,
    /// The native handle-bound publication was denied without a collision result.
    AccessDenied,
    /// The destination publication is durable, but the source was retained.
    /// Retrying this request blindly would create a second destination.
    Partial(String),
    /// Staging cleanup failed before publication; the requested destination is untouched.
    PrePublishPartial(String),
    /// The platform may have mutated the item but could not prove its final receipt.
    Unknown(String),
    /// Directory publication is uncertain; compact typed evidence is durable
    /// but carries no per-entry cleanup or Undo authority.
    UnknownDirectoryPublication(Box<super::model::CopyDirectoryPublicationEvidence>),
    /// E7's exact first-intent refusal was followed by a durable NoEffect ACK.
    DirectoryMoveReplaceNoEffect { cancelled: bool },
    DirectoryCopyStageRetained {
        evidence: super::model::CopyDirectoryStageEvidence,
        cause: Box<CopyError>,
    },
    Io(String),
}

/// Tagged facts produced by a detached worker. The caller delivers these to
/// `OperationService`, which remains the only lifecycle owner.
#[derive(Clone, Debug)]
pub enum ExecutorFact {
    OrdinaryCopySourceKind {
        token: AttemptToken,
        item: super::model::ItemId,
        kind: OrdinaryCopySourceKind,
    },
    /// Candidate evidence is process-local and must precede the matching terminal on this FIFO.
    CreatedObjectUndoCandidate {
        token: AttemptToken,
        item: super::model::ItemId,
        receipt: super::model::CreatedObjectUndoReceipt,
    },
    DirectoryCopyUndoCandidate {
        token: AttemptToken,
        item: super::model::ItemId,
        receipt: super::model::DirectoryCopyUndoReceipt,
    },
    MoveUndoCandidate {
        token: AttemptToken,
        item: super::model::ItemId,
        receipt: super::model::MoveUndoReceipt,
    },
    BatchRenamePlanIntent {
        token: AttemptToken,
        plan: Vec<super::batch_rename::RenameTarget>,
    },
    /// Worker-observed Submit-path identities. Runtime commits this before
    /// the ordered terminal fact, allowing retries to retain the same guard.
    MoveGuardResolved {
        token: AttemptToken,
        guard: super::model::MoveIdentityGuard,
    },
    /// A guarded direct same-volume Move adapter returned an OS I/O error.
    GuardedMoveNoEffectProof {
        token: AttemptToken,
        proof: super::model::GuardedMoveNoEffectProof,
    },
    CrossVolumeMoveNoEffectProof {
        token: AttemptToken,
        proof: super::model::CrossVolumeMoveNoEffectProof,
    },
    PublishedMoveSourceRetained {
        token: AttemptToken,
        proof: super::model::PublishedMoveSourceRetainedProof,
    },
    PublishedMoveDisposition {
        token: AttemptToken,
        proof: super::model::PublishedMoveDispositionProof,
    },
    CopyFailureProvenance {
        token: AttemptToken,
        provenance: super::model::CopyFailureProvenance,
    },
    CopyDirectoryRetryProvenance {
        token: AttemptToken,
        provenance: super::model::CopyDirectoryRetryProvenance,
    },
    /// Non-cloneable process-local authority emitted only when a fresh Copy
    /// stage could not be discarded through its still-open original handle.
    CopyStageCustody {
        custody: super::runtime::CopyStageCustodyTransfer,
    },
    /// A recycle receipt is opaque capability data.  It reaches the runtime
    /// before terminal so the runtime, not the view, owns the eventual undo.
    TrashReceipt {
        token: AttemptToken,
        item: super::model::ItemId,
        receipt: super::trash::TrashReceipt,
    },
    /// The worker waits for a durable-journal acknowledgement before it
    /// displaces an existing Replace target into this owned backup path.
    ReplaceBackupIntent {
        token: AttemptToken,
        item: super::model::ItemId,
        source: PathBuf,
        destination: PathBuf,
        backup: PathBuf,
    },
    CrossVolumeMoveReplacePhaseIntent {
        token: AttemptToken,
        ledger: super::journal::JournalCrossVolumeMoveReplace,
    },
    DirectoryMoveReplaceRenamePhaseIntent {
        token: AttemptToken,
        conflict_token: super::model::ConflictToken,
        ledger: super::journal::JournalDirectoryMoveReplaceRename,
    },
    DirectoryMoveReplaceRenameCleanupIntent { token: AttemptToken, item: ItemId, intent: super::journal::JournalDirectoryMoveReplaceRenameChildIntent },
    DirectoryMoveReplaceRenameCleanupDisposition { token: AttemptToken, item: ItemId, intent: super::journal::JournalDirectoryMoveReplaceRenameChildIntent },
    DirectoryMoveReplaceRenameBackupComplete { token: AttemptToken, item: ItemId },
    RenameStageIntent {
        token: AttemptToken,
        item: super::model::ItemId,
        source: PathBuf,
        stage: PathBuf,
        destination: PathBuf,
        source_identity: String,
        parent_identity: String,
        phase: u8,
    },
    /// Exact archive stage/output facts; the worker waits for the serial journal ack.
    ZipCreateLedgerIntent {
        token: AttemptToken,
        ledger: super::journal::JournalZipCreate,
    },
    SevenZipCreateLedgerIntent {
        token: AttemptToken,
        ledger: super::journal::JournalSevenZipHandleCreate,
    },
    ZipExtractLedgerIntent {
        token: AttemptToken,
        item: super::model::ItemId,
        ledger: super::journal::JournalZipExtract,
    },
    SevenZipExtractBatchIntent {
        token: AttemptToken,
        batch: super::journal::JournalSevenZipExtractBatch,
    },
    DirectoryMoveManifestIntent {
        token: AttemptToken,
        ledger: super::journal::JournalDirectoryMove,
    },
    DirectoryMoveChildIntent {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryChildIntent,
    },
    DirectoryMoveChildDisposition {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryChildIntent,
    },
    DirectoryMoveRootIntent {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryRootIntent,
    },
    DirectoryMoveRootDisposition {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryRootIntent,
    },
    DirectoryMoveComplete {
        token: AttemptToken,
        item: ItemId,
    },
    DirectoryMoveReplacePhaseIntent {
        token: AttemptToken,
        item: ItemId,
        phase: super::journal::DirectoryMoveReplacePhase,
    },
    DirectoryMoveReplaceBackupIntent {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryMoveReplaceChildIntent,
    },
    DirectoryMoveReplaceBackupDisposition {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryMoveReplaceChildIntent,
    },
    DirectoryMoveReplaceBackupComplete {
        token: AttemptToken,
        item: ItemId,
    },
    CopyReplaceCleanupManifestIntent {
        token: AttemptToken,
        ledger: super::journal::JournalCopyReplaceCleanup,
    },
    CopyReplaceCleanupChildIntent {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalCopyReplaceChildIntent,
    },
    CopyReplaceCleanupChildDisposition {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalCopyReplaceChildIntent,
    },
    CopyReplaceCleanupComplete {
        token: AttemptToken,
        item: ItemId,
    },
    BatchRenameItemCompleted {
        token: AttemptToken,
        item: super::model::ItemId,
        source: PathBuf,
        destination: PathBuf,
        source_identity: String,
        parent_identity: String,
    },
    Conflict {
        token: AttemptToken,
        notice: super::model::ConflictNotice,
    },
    Progress {
        token: AttemptToken,
        update: ProgressUpdate,
    },
    Terminal {
        token: AttemptToken,
        report: TerminalReport,
    },
}

/// Control sent by the shell only after `OperationService` accepted the exact
/// conflict token.  A blocked worker also receives Cancel so it never waits on
/// a dialog after Escape/cancel has made the attempt terminal-bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerControl {
    BatchRenamePlanJournaledExact {
        token: AttemptToken,
        plan: Vec<super::batch_rename::RenameTarget>,
    },
    Resolve {
        decision: super::model::ConflictDecision,
        apply_to_remaining: bool,
    },
    DirectoryMoveReplaceRenameApproved {
        token: super::model::ConflictToken,
        candidate: super::model::DirectoryMoveReplaceRenameCandidate,
    },
    ReplaceBackupJournaled {
        backup: PathBuf,
    },
    ReplaceBackupJournaledExact {
        token: AttemptToken,
        item: super::model::ItemId,
        source: PathBuf,
        destination: PathBuf,
        backup: PathBuf,
    },
    CrossVolumeMoveReplacePhaseJournaledExact {
        token: AttemptToken,
        ledger: super::journal::JournalCrossVolumeMoveReplace,
    },
    CrossVolumeMoveReplacePhaseJournalFailedExact {
        token: AttemptToken,
        ledger: super::journal::JournalCrossVolumeMoveReplace,
    },
    DirectoryMoveReplaceRenamePhaseJournaledExact {
        token: AttemptToken,
        conflict_token: super::model::ConflictToken,
        ledger: super::journal::JournalDirectoryMoveReplaceRename,
    },
    DirectoryMoveReplaceRenamePhaseJournalFailedExact {
        token: AttemptToken,
        conflict_token: super::model::ConflictToken,
        ledger: super::journal::JournalDirectoryMoveReplaceRename,
    },
    DirectoryMoveReplaceRenameCleanupIntentJournaledExact { token: AttemptToken, item: ItemId, intent: super::journal::JournalDirectoryMoveReplaceRenameChildIntent },
    DirectoryMoveReplaceRenameCleanupReceiptJournaledExact { token: AttemptToken, item: ItemId, intent: super::journal::JournalDirectoryMoveReplaceRenameChildIntent },
    DirectoryMoveReplaceRenameBackupCompleteJournaledExact { token: AttemptToken, item: ItemId },
    RenameStageJournaledExact {
        token: AttemptToken,
        item: super::model::ItemId,
        source: PathBuf,
        stage: PathBuf,
        destination: PathBuf,
        source_identity: String,
        parent_identity: String,
        phase: u8,
    },
    ZipCreateLedgerJournaledExact {
        token: AttemptToken,
        ledger: super::journal::JournalZipCreate,
    },
    ZipCreateLedgerJournalFailedExact {
        token: AttemptToken,
        ledger: super::journal::JournalZipCreate,
    },
    SevenZipExtractBatchJournaledExact {
        token: AttemptToken,
        batch: super::journal::JournalSevenZipExtractBatch,
    },
    SevenZipExtractBatchObservedExact {
        token: AttemptToken,
        batch: super::journal::JournalSevenZipExtractBatch,
    },
    SevenZipExtractBatchCleanupJournaledExact {
        token: AttemptToken,
        batch: super::journal::JournalSevenZipExtractBatch,
        item: super::model::ItemId,
        target: super::runtime::SevenZipExtractCleanupTarget,
    },
    SevenZipExtractBatchJournalFailedExact {
        token: AttemptToken,
        batch: super::journal::JournalSevenZipExtractBatch,
    },
    SevenZipExtractTerminalJournaledExact {
        token: AttemptToken,
        batch: Option<super::journal::JournalSevenZipExtractBatch>,
    },
    SevenZipCreateLedgerJournaledExact {
        token: AttemptToken,
        ledger: super::journal::JournalSevenZipHandleCreate,
    },
    SevenZipCreateLedgerJournalFailedExact {
        token: AttemptToken,
        ledger: super::journal::JournalSevenZipHandleCreate,
    },
    SevenZipCreateLedgerJournaledCancelledExact {
        token: AttemptToken,
        ledger: super::journal::JournalSevenZipHandleCreate,
    },
    SevenZipCreateCleanupJournaledExact {
        token: AttemptToken,
        ledger: super::journal::JournalSevenZipHandleCreate,
        target: SevenZipCreateCleanupTarget,
    },
    ZipExtractLedgerJournaledExact {
        token: AttemptToken,
        item: super::model::ItemId,
        ledger: super::journal::JournalZipExtract,
    },
    ZipExtractLedgerJournalFailedExact {
        token: AttemptToken,
        item: super::model::ItemId,
        ledger: super::journal::JournalZipExtract,
    },
    BatchRenameItemJournaledExact {
        token: AttemptToken,
        item: super::model::ItemId,
        source: PathBuf,
        destination: PathBuf,
    },
    DirectoryMoveManifestJournaledExact {
        token: AttemptToken,
        ledger: super::journal::JournalDirectoryMove,
    },
    DirectoryMoveChildIntentJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryChildIntent,
    },
    DirectoryMoveChildReceiptJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryChildIntent,
    },
    DirectoryMoveRootIntentJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryRootIntent,
    },
    DirectoryMoveRootReceiptJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryRootIntent,
    },
    DirectoryMoveCompleteJournaledExact {
        token: AttemptToken,
        item: ItemId,
    },
    DirectoryMoveReplacePhaseJournaledExact {
        token: AttemptToken,
        item: ItemId,
        phase: super::journal::DirectoryMoveReplacePhase,
    },
    DirectoryMoveReplacePhaseJournalFailedExact {
        token: AttemptToken,
        item: ItemId,
        phase: super::journal::DirectoryMoveReplacePhase,
    },
    DirectoryMoveReplaceBackupIntentJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryMoveReplaceChildIntent,
    },
    DirectoryMoveReplaceBackupReceiptJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalDirectoryMoveReplaceChildIntent,
    },
    DirectoryMoveReplaceBackupCompleteJournaledExact {
        token: AttemptToken,
        item: ItemId,
    },
    CopyReplaceCleanupManifestJournaledExact {
        token: AttemptToken,
        ledger: super::journal::JournalCopyReplaceCleanup,
    },
    CopyReplaceCleanupChildIntentJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalCopyReplaceChildIntent,
    },
    CopyReplaceCleanupChildReceiptJournaledExact {
        token: AttemptToken,
        item: ItemId,
        intent: super::journal::JournalCopyReplaceChildIntent,
    },
    CopyReplaceCleanupCompleteJournaledExact {
        token: AttemptToken,
        item: ItemId,
    },
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SevenZipCreateCleanupTarget {
    Candidate,
    Stage,
}

pub(super) fn wait_for_cross_volume_move_replace_phase_ack(
    token: AttemptToken,
    ledger: &super::journal::JournalCrossVolumeMoveReplace,
    controls: &Receiver<WorkerControl>,
) -> Result<(), CopyError> {
    loop {
        match controls.recv() {
            // Cancellation remains set in the shared flag. Consume its queued
            // wakeup but wait for the exact journal result so a committed
            // receipt is not lost from historical state.
            Ok(WorkerControl::Cancel) => continue,
            Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournaledExact {
                token: ack_token,
                ledger: ack_ledger,
            }) if ack_token == token && ack_ledger == *ledger => {
                #[cfg(all(test, target_os = "windows"))]
                tests::checkpoint_cross_volume_replace_after_ack(ledger);
                return Ok(());
            }
            Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournalFailedExact {
                token: failed_token,
                ledger: failed_ledger,
            }) if failed_token == token && failed_ledger == *ledger => {
                return Err(CopyError::Unsupported);
            }
            _ => return Err(CopyError::Unsupported),
        }
    }
}

/// Replace needs to report both staging progress and the durable backup
/// barrier in order. Keeping them on one callback lets the worker wait for the
/// journal acknowledgement without pausing the operation card for a large
/// source file.
enum ReplaceEvent {
    BackupIntent(ExecutorFact),
    BytesCopied(u64),
}

pub fn execute_prepare(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: impl FnMut(ExecutorFact),
) {
    execute_prepare_inner_with_lease(command, cancelled, controls, None, emit, || {}, trash_file);
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_trash_adapter_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: impl FnMut(ExecutorFact),
    mut adapter: impl FnMut(&Path) -> Result<super::trash::TrashReceipt, super::trash::TrashError>,
) {
    execute_prepare_inner_with_lease(
        command,
        cancelled,
        controls,
        None,
        emit,
        || {},
        |source, accepted, tag, cancelled| {
            trash_file_with_adapter(source, accepted, tag, cancelled, || adapter(source))
        },
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_trash_identity_adapter_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: impl FnMut(ExecutorFact),
    mut adapter: impl FnMut(
        &Path,
        &crate::directory::EntrySourceIdentity,
    ) -> Result<super::trash::TrashReceipt, super::trash::TrashError>,
) {
    execute_prepare_inner_with_lease(
        command,
        cancelled,
        controls,
        None,
        emit,
        || {},
        |source, accepted, tag, cancelled| {
            trash_file_with_adapter(source, accepted, tag, cancelled, || {
                adapter(source, &accepted.identity)
            })
        },
    );
}

pub fn execute_prepare_with_source_cleanup_lease(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    lease: Option<super::runtime::SourceCleanupLease>,
    emit: impl FnMut(ExecutorFact),
) {
    execute_prepare_inner_with_lease(command, cancelled, controls, lease, emit, || {}, trash_file);
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_copy_stage_cleanup_failure_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    lease: Option<super::runtime::SourceCleanupLease>,
    mut emit: impl FnMut(ExecutorFact),
) {
    // Faults are thread-local, so arm the existing cleanup fault on the real
    // worker thread and cancel only after its first accepted Copy progress.
    windows::set_copy_test_faults(true, false);
    execute_prepare_inner_with_lease(
        command,
        cancelled,
        controls,
        lease,
        |fact| {
            let cancel_after_progress = matches!(fact, ExecutorFact::Progress { .. });
            emit(fact);
            if cancel_after_progress {
                cancelled.store(true, Ordering::Release);
            }
        },
        || {},
        trash_file,
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_copy_stage_cleanup_failure_for_item_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    lease: Option<super::runtime::SourceCleanupLease>,
    target_item: super::model::ItemId,
    mut emit: impl FnMut(ExecutorFact),
) {
    // Thread-local faults must be armed on the worker that owns the stage.
    windows::set_copy_test_faults(true, false);
    execute_prepare_inner_with_lease(
        command,
        cancelled,
        controls,
        lease,
        |fact| {
            let cancel_after_target_progress = matches!(
                &fact,
                ExecutorFact::Progress { update, .. }
                    if update.current_item == Some(target_item)
                        && update.counters.processed_bytes > 0
            );
            emit(fact);
            if cancel_after_target_progress {
                cancelled.store(true, Ordering::Release);
            }
        },
        || {},
        trash_file,
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_after_copy_source_replacement_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    source: PathBuf,
    preserved_original: PathBuf,
    replacement_bytes: Vec<u8>,
    emit: impl FnMut(ExecutorFact),
) {
    // Retry admission is complete; replace only the requested source before
    // production executor guard validation begins.
    std::fs::rename(&source, preserved_original).expect("move inspected source aside");
    std::fs::write(&source, replacement_bytes).expect("write replacement at inspected source path");
    execute_prepare(command, cancelled, controls, emit);
}

pub(crate) fn execute_prepare_with_archive_provider(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    lease: Option<super::runtime::SourceCleanupLease>,
    copy_stage_cleanup_lease: Option<super::runtime::CopyStageCleanupLease>,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    seven_zip_extract_attempt: Option<super::journal::JournalSevenZipExtractAttemptId>,
    emit: impl FnMut(ExecutorFact),
) {
    execute_prepare_inner_with_lease_and_provider(
        command,
        cancelled,
        controls,
        lease,
        copy_stage_cleanup_lease,
        provider,
        seven_zip_extract_attempt,
        emit,
        || {},
        trash_file,
        ExecutePrepareOptions::default(),
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_directory_move_replace_enabled_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: impl FnMut(ExecutorFact),
) {
    execute_prepare_inner_with_lease_and_provider(
        command,
        cancelled,
        controls,
        None,
        None,
        None,
        None,
        emit,
        || {},
        trash_file,
        ExecutePrepareOptions {
            directory_move_replace_enabled: true,
            directory_move_replace_rename_enabled: false,
            cross_volume_regular_file_move_replace_enabled: false,
            ..ExecutePrepareOptions::default()
        },
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_cross_volume_file_move_replace_enabled_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: impl FnMut(ExecutorFact),
) {
    execute_prepare_inner_with_lease_and_provider(
        command,
        cancelled,
        controls,
        None,
        None,
        None,
        None,
        emit,
        || {},
        trash_file,
        ExecutePrepareOptions {
            directory_move_replace_enabled: false,
            directory_move_replace_rename_enabled: false,
            cross_volume_regular_file_move_replace_enabled: true,
            ..ExecutePrepareOptions::default()
        },
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_cross_volume_move_source_disposition_failure_for_test(
    command: ExecutorCommand,
    expected_token: AttemptToken,
    expected_item: ItemId,
    source: PathBuf,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    lease: Option<super::runtime::SourceCleanupLease>,
    emit: impl FnMut(ExecutorFact),
) {
    let (command_token, request) = match &command {
        ExecutorCommand::Prepare { token, request, .. } => (*token, request),
        _ => panic!("source disposition injection requires a Prepare command"),
    };
    assert_eq!(command_token, expected_token, "test hook is bound to the admitted attempt");
    assert_eq!(request.kind, OperationKind::Move, "source disposition injection is Move-only");
    assert_eq!(request.sources.get(expected_item.0 as usize), Some(&source), "test hook source must occupy the exact admitted item slot");
    windows::with_cross_volume_move_source_disposition_failure_for_exact_item_for_test(
        expected_token,
        expected_item,
        &source,
        || execute_prepare_with_source_cleanup_lease(command, cancelled, controls, lease, emit),
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_cross_volume_file_move_replace_partial_stage_failure_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    observed_stage: std::sync::mpsc::SyncSender<(PathBuf, Vec<u8>)>,
    emit: impl FnMut(ExecutorFact),
) {
    // この故障はstageを所有した実workerのcopy loop内でだけ消費する。
    tests::arm_cross_volume_replace_partial_stage_write_failure_for_test(observed_stage);
    execute_prepare_with_cross_volume_file_move_replace_enabled_for_test(
        command, cancelled, controls, emit,
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_cross_volume_file_move_replace_stage_flush_failure_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    observed_stage: std::sync::mpsc::SyncSender<(PathBuf, Vec<u8>)>,
    emit: impl FnMut(ExecutorFact),
) {
    tests::arm_cross_volume_replace_stage_flush_failure_for_test(observed_stage);
    execute_prepare_with_cross_volume_file_move_replace_enabled_for_test(
        command, cancelled, controls, emit,
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_archive_provider_and_image_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    provider: crate::archive_provider::ArchiveProviderOwner,
    image: crate::archive_provider::application_image::ApplicationImageLease,
    mut emit: impl FnMut(ExecutorFact),
) {
    let ExecutorCommand::Prepare { token, request, .. } = command else {
        return;
    };
    if request.kind != OperationKind::Create7z {
        return;
    }
    seven_zip_create::execute_with_image_for_test(
        token, &request, provider, image, cancelled, controls, &mut emit,
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_seven_zip_extract_with_image_for_test(
    token: AttemptToken,
    request: &super::model::OperationRequest,
    attempt: super::journal::JournalSevenZipExtractAttemptId,
    provider: crate::archive_provider::ArchiveProviderOwner,
    image: crate::archive_provider::application_image::ApplicationImageLease,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    mut emit: impl FnMut(ExecutorFact),
) {
    if request.kind != OperationKind::Extract7z {
        return;
    }
    seven_zip_extract::execute(
        token,
        request,
        Some(attempt),
        Some(provider),
        Some(image),
        cancelled,
        controls,
        &mut emit,
    );
}

fn execute_prepare_inner(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: impl FnMut(ExecutorFact),
    after_destination_probe: impl FnMut(),
) {
    execute_prepare_inner_with_lease(
        command,
        cancelled,
        controls,
        None,
        emit,
        after_destination_probe,
        trash_file,
    );
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_destination_probe_callback_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    source_cleanup_lease: Option<super::runtime::SourceCleanupLease>,
    emit: impl FnMut(ExecutorFact),
    after_destination_probe: impl FnMut(),
) {
    execute_prepare_inner_with_lease(
        command,
        cancelled,
        controls,
        source_cleanup_lease,
        emit,
        after_destination_probe,
        trash_file,
    );
}

fn execute_prepare_inner_with_lease(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    source_cleanup_lease: Option<super::runtime::SourceCleanupLease>,
    emit: impl FnMut(ExecutorFact),
    after_destination_probe: impl FnMut(),
    trash_adapter: impl FnMut(
        &Path,
        &super::model::TrashSourceGuard,
        Option<u32>,
        &AtomicBool,
    ) -> Result<super::trash::TrashReceipt, CopyError>,
) {
    execute_prepare_inner_with_lease_and_provider(
        command,
        cancelled,
        controls,
        source_cleanup_lease,
        None,
        None,
        None,
        emit,
        after_destination_probe,
        trash_adapter,
        ExecutePrepareOptions::default(),
    );
}

#[derive(Default)]
struct ExecutePrepareOptions {
    directory_move_replace_enabled: bool,
    directory_move_replace_rename_enabled: bool,
    cross_volume_regular_file_move_replace_enabled: bool,
    #[cfg(all(test, windows))]
    directory_move_replace_rename_test_hook: Option<DirectoryMoveReplaceRenameTestHook>,
}

#[cfg(all(test, windows))]
pub(crate) struct DirectoryMoveReplaceRenameTestHook {
    pub(crate) before_publish: Box<dyn Fn(&Path) + Send + Sync>,
    pub(crate) publish_result: std::sync::mpsc::SyncSender<Result<(), super::rename::RenameError>>,
    pub(crate) before_root_disposition: Option<Box<dyn Fn(&Path) + Send + Sync>>,
    // ACK送信側の観測ではworkerと競合するため、executor側からACK済みphaseを通知する。
    pub(crate) after_phase_ack: Option<Box<dyn Fn(super::journal::DirectoryMoveReplaceRenamePhase) + Send + Sync>>,
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_copy_directory_staging_failure_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    target_token: super::model::AttemptToken,
    target_directory_item: super::model::ItemId,
    target_file_item: super::model::ItemId,
    mut emit: impl FnMut(ExecutorFact),
) {
    let token = match &command {
        ExecutorCommand::Prepare { token, .. } => *token,
        _ => panic!("directory staging fault requires a Prepare command"),
    };
    assert_eq!(token, target_token, "fault is armed for this exact attempt");
    let ((), consumed) = windows::with_copy_directory_staging_failure_for_test(
        target_token,
        target_directory_item,
        || {
            let ((), cleanup_failure_consumed) = windows::with_copy_test_faults_for_test(
                true,
                false,
                || {
                    execute_prepare(command, cancelled, controls, |fact| {
                        let cancel_after_target_progress = matches!(
                            &fact,
                            ExecutorFact::Progress { update, .. }
                                if update.current_item == Some(target_file_item)
                                    && update.counters.processed_bytes > 0
                        );
                        emit(fact);
                        if cancel_after_target_progress {
                            cancelled.store(true, Ordering::Release);
                        }
                    });
                },
            );
            assert!(cleanup_failure_consumed, "targeted file-stage cleanup fault was consumed");
        },
    );
    assert!(consumed, "targeted directory staging fault was consumed exactly once");
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_recovery_copy_directory_staging_failure_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    target_token: super::model::AttemptToken,
    target_directory_item: super::model::ItemId,
    emit: impl FnMut(ExecutorFact),
) {
    let token = match &command {
        ExecutorCommand::Prepare { token, .. } => *token,
        _ => panic!("Recovery Copy staging fault requires a Prepare command"),
    };
    assert_eq!(token, target_token, "fault is armed for this exact attempt");
    let ((), consumed) = windows::with_copy_directory_staging_failure_for_test(
        target_token,
        target_directory_item,
        || execute_prepare(command, cancelled, controls, emit),
    );
    assert!(consumed, "targeted Recovery Copy staging fault was consumed exactly once");
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_recovery_copy_directory_stage_write_lock_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    target_token: super::model::AttemptToken,
    target_directory_item: super::model::ItemId,
    on_locked_stage_write: impl FnOnce(&std::fs::File) + Send + 'static,
    emit: impl FnMut(ExecutorFact),
) {
    let token = match &command {
        ExecutorCommand::Prepare { token, .. } => *token,
        _ => panic!("Recovery Copy stage write-lock requires a Prepare command"),
    };
    assert_eq!(token, target_token, "lock hook is armed for this exact attempt");
    windows::COPY_DIRECTORY_STAGE_WRITE_ERROR_TEST.with(|slot| slot.set(None));
    let ((), consumed) = windows::with_copy_directory_stage_write_lock_for_test(
        target_token,
        target_directory_item,
        on_locked_stage_write,
        || execute_prepare(command, cancelled, controls, emit),
    );
    assert!(consumed, "targeted Recovery Copy stage write-lock hook was consumed exactly once");
    let write_error = windows::COPY_DIRECTORY_STAGE_WRITE_ERROR_TEST.with(std::cell::Cell::get);
    assert_eq!(write_error, Some(33), "the actual staged-file write must fail with ERROR_LOCK_VIOLATION");
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_recovery_copy_directory_progress_gate_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    target_token: super::model::AttemptToken,
    target_directory_item: super::model::ItemId,
    progress_accepted: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
    mut emit: impl FnMut(ExecutorFact),
) {
    let token = match &command {
        ExecutorCommand::Prepare { token, .. } => *token,
        _ => panic!("Recovery Copy progress gate requires a Prepare command"),
    };
    assert_eq!(token, target_token, "gate is attached to this exact attempt");
    execute_prepare(command, cancelled, controls, |fact| {
        let gate_after_positive_directory_progress = matches!(
            &fact,
            ExecutorFact::Progress { token, update }
                if *token == target_token
                    && update.current_item == Some(target_directory_item)
                    && update.counters.processed_bytes > 0
        );
        emit(fact);
        if gate_after_positive_directory_progress {
            progress_accepted
                .send(())
                .expect("Recovery Copy test is waiting for positive directory progress");
            let _ = release.recv();
        }
    });
}

#[cfg(all(test, windows))]
pub(crate) fn execute_prepare_with_directory_move_replace_rename_enabled_for_test(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    test_hook: Option<DirectoryMoveReplaceRenameTestHook>,
    emit: impl FnMut(ExecutorFact),
) {
    execute_prepare_inner_with_lease_and_provider(
        command,
        cancelled,
        controls,
        None,
        None,
        None,
        None,
        emit,
        || {},
        trash_file,
        ExecutePrepareOptions {
            directory_move_replace_enabled: false,
            directory_move_replace_rename_enabled: true,
            cross_volume_regular_file_move_replace_enabled: false,
            directory_move_replace_rename_test_hook: test_hook,
        },
    );
}

fn execute_prepare_inner_with_lease_and_provider(
    command: ExecutorCommand,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    mut source_cleanup_lease: Option<super::runtime::SourceCleanupLease>,
    mut copy_stage_cleanup_lease: Option<super::runtime::CopyStageCleanupLease>,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    seven_zip_extract_attempt: Option<super::journal::JournalSevenZipExtractAttemptId>,
    mut emit: impl FnMut(ExecutorFact),
    mut after_destination_probe: impl FnMut(),
    mut trash_adapter: impl FnMut(
        &Path,
        &super::model::TrashSourceGuard,
        Option<u32>,
        &AtomicBool,
    ) -> Result<super::trash::TrashReceipt, CopyError>,
    // テスト用hookを一度だけ取り出すため、test buildでは可変になる。
    #[allow(unused_mut)]
    mut options: ExecutePrepareOptions,
) {
    let ExecutorCommand::Prepare {
        token,
        mut request,
        retry_items,
        copy_retry_guards,
        copy_directory_retry_guards,
        cross_volume_move_retry_guards,
    } = command
    else {
        return;
    };
    if request.kind == OperationKind::CreateShortcut {
        create_shortcut::execute(token, &request, cancelled, &mut emit);
        return;
    }
    if let Some(sources) = request.virtual_source.take() {
        virtual_copy::execute_virtual_copy(
            token,
            request,
            sources.items,
            cancelled,
            controls,
            &mut emit,
        );
        return;
    }
    if request.kind == OperationKind::ExternalMoveCleanup {
        execute_external_move_cleanup(
            token,
            &request,
            &retry_items,
            source_cleanup_lease.take(),
            cancelled,
            &mut emit,
        );
        return;
    }
    if request.kind == OperationKind::CopyStageCleanup {
        execute_copy_stage_cleanup(
            token,
            &request,
            copy_stage_cleanup_lease.take(),
            cancelled,
            &mut emit,
        );
        return;
    }
    if request.kind == OperationKind::CreateFolder {
        create_folder::execute(token, &request, cancelled, &mut emit);
        return;
    }
    if request.kind == OperationKind::CreateFile {
        create_file::execute(token, &request, cancelled, &mut emit);
        return;
    }
    if request.kind == OperationKind::CreateZip {
        zip_create::execute(token, &request, cancelled, controls, &mut emit);
        return;
    }
    if request.kind == OperationKind::Create7z {
        #[cfg(windows)]
        seven_zip_create::execute(token, &request, provider, cancelled, controls, &mut emit);
        #[cfg(not(windows))]
        emit(ExecutorFact::Terminal {
            token,
            report: seven_zip_not_implemented_report(),
        });
        return;
    }
    if request.kind == OperationKind::ExtractZip {
        zip_extract::execute(token, &request, cancelled, controls, &mut emit);
        return;
    }
    if request.kind == OperationKind::Extract7z {
        #[cfg(windows)]
        seven_zip_extract::execute(
            token,
            &request,
            seven_zip_extract_attempt,
            provider,
            None,
            cancelled,
            controls,
            &mut emit,
        );
        #[cfg(not(windows))]
        emit(ExecutorFact::Terminal {
            token,
            report: seven_zip_extract_not_implemented_report(&request),
        });
        return;
    }
    if !matches!(
        request.kind,
        OperationKind::Copy
            | OperationKind::Move
            | OperationKind::CreateSymbolicLink
            | OperationKind::CreateJunction
            | OperationKind::Trash
            | OperationKind::Rename
            | OperationKind::BatchRename
            | OperationKind::BatchRenameUndo
    ) {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    if matches!(
        request.kind,
        OperationKind::BatchRename | OperationKind::BatchRenameUndo
    ) {
        let valid = request.batch_rename_plan.as_ref().is_some_and(|plan| {
            !plan.is_empty()
                && plan.len() == request.sources.len()
                && plan.iter().enumerate().all(|(index, target)| {
                    request.sources.get(index) == Some(&target.source)
                        && target.source.parent() == target.destination.parent()
                        && target.destination.file_name().is_some_and(|name| {
                            super::rename::is_single_name(&name.to_string_lossy())
                        })
                        && !plan.iter().enumerate().any(|(other_index, other)| {
                            other_index != index
                                && target
                                    .destination
                                    .file_name()
                                    .zip(other.source.file_name())
                                    .is_some_and(|(dest, src)| {
                                        dest.to_string_lossy()
                                            .eq_ignore_ascii_case(&src.to_string_lossy())
                                    })
                        })
                        && !plan.iter().enumerate().any(|(other_index, other)| {
                            other_index < index
                                && target
                                    .destination
                                    .file_name()
                                    .zip(other.destination.file_name())
                                    .is_some_and(|(a, b)| {
                                        a.to_string_lossy()
                                            .eq_ignore_ascii_case(&b.to_string_lossy())
                                    })
                        })
                })
        });
        if !valid {
            emit(ExecutorFact::Terminal {
                token,
                report: rejected(),
            });
            return;
        }
        if matches!(
            request.kind,
            OperationKind::BatchRename | OperationKind::BatchRenameUndo
        ) {
            let plan = request.batch_rename_plan.clone().unwrap_or_default();
            emit(ExecutorFact::BatchRenamePlanIntent {
                token,
                plan: plan.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::BatchRenamePlanJournaledExact {
                    token: ack_token,
                    plan: ack_plan,
                }) if ack_token == token && ack_plan == plan => {}
                _ => {
                    emit(ExecutorFact::Terminal {
                        token,
                        report: rejected(),
                    });
                    return;
                }
            }
        }
    }
    let destination = match request.kind {
        OperationKind::Copy
        | OperationKind::Move
        | OperationKind::CreateSymbolicLink
        | OperationKind::CreateJunction
        | OperationKind::Rename => match request.destination.clone() {
            Some(destination) => Some(destination),
            None => {
                emit(ExecutorFact::Terminal {
                    token,
                    report: rejected(),
                });
                return;
            }
        },
        OperationKind::Trash => None,
        OperationKind::BatchRename => None,
        OperationKind::BatchRenameUndo => None,
        OperationKind::CreateFolder => None,
        OperationKind::CreateFile => None,
        OperationKind::CreateShortcut => unreachable!("handled before destination validation"),
        OperationKind::CreateZip => unreachable!("handled before destination validation"),
        OperationKind::Create7z => unreachable!("handled before destination validation"),
        OperationKind::ExtractZip => unreachable!("handled before destination validation"),
        OperationKind::Extract7z => unreachable!("handled before destination validation"),
        OperationKind::ExternalMoveCleanup => unreachable!("handled before destination validation"),
        OperationKind::CopyStageCleanup => unreachable!("handled before destination validation"),
    };
    if !trash_source_guards_are_well_formed(&request) {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    // Ordinary case-only Rename needs its accepted move identity guard.
    // A reparse guard independently admits the established non-case-only link
    // route, and may accompany that identity guard for case-only link renames.
    if request.source_cleanup_identity_guard.is_some()
        || (request.copy_destination_identity_guard.is_some()
            && request.kind != OperationKind::Copy)
        || (request.move_destination_identity_guard.is_some()
            && (request.kind != OperationKind::Move || request.move_identity_guard.is_some()))
        || request.move_replace_source_evidence.as_ref().is_some_and(|evidence| {
            request.kind != OperationKind::Move
                || request.sources.len() != evidence.len()
                || evidence.is_empty()
                || request.virtual_source.is_some()
                || request.move_identity_guard.is_some()
                || request.move_destination_identity_guard.is_none()
                || evidence.iter().any(|proof| {
                    proof.source_identity.windows_parts().zip(
                        request.move_destination_identity_guard.as_ref().and_then(|identity| identity.windows_parts()),
                    ).is_none_or(|((source_volume, _), (destination_volume, _))| source_volume != destination_volume)
                })
        })
        || (request.initial_conflict_policy.is_some()
            && !(request.kind == OperationKind::Copy
                && request.initial_conflict_policy == Some(super::model::ConflictDecision::KeepBoth)
                && request.move_identity_guard.as_ref().is_some_and(|identity_guard| {
                    request.copy_destination_identity_guard.as_ref() == Some(&identity_guard.destination_identity)
                        && identity_guard.source_identities.len() == request.sources.len()
                        && !request.sources.is_empty()
                })))
        || request.move_identity_guard.as_ref().is_some_and(|guard| {
            (request.kind != OperationKind::Move
                && request.kind != OperationKind::Rename
                && !(request.kind == OperationKind::Copy
                    && request.virtual_source.is_none()
                    && request.initial_conflict_policy == Some(super::model::ConflictDecision::KeepBoth)
                    && request.copy_destination_identity_guard.as_ref() == Some(&guard.destination_identity)
                    && request.sources.len() == guard.source_identities.len()
                    && !request.sources.is_empty()))
                || (request.kind == OperationKind::Move
                    && !guard.source_identities.is_empty()
                    && guard.source_identities.len() != request.sources.len())
                || (request.kind == OperationKind::Rename
                    && (request.sources.len() != 1 || guard.source_identities.len() != 1))
                || (request.kind == OperationKind::Copy
                    && (guard.source_identities.len() != request.sources.len()
                        || guard.source_identities.iter().any(|identity| identity.windows_parts().is_none())))
        })
        || request.clipboard_move_source_evidence.as_ref().is_some_and(|evidence| {
            request.kind != OperationKind::Move
                || request.sources.len() != evidence.len()
                || evidence.is_empty()
                || request.virtual_source.is_some()
                || request.move_identity_guard.is_some()
                || request.move_destination_identity_guard.is_none()
                || evidence.iter().zip(&request.sources).any(|(accepted, source)| {
                    accepted.source != *source
                        || accepted.source_identity.windows_parts().is_none()
                        || accepted.reparse_tag.is_some_and(|tag| !matches!(tag, 0xA000_000C | 0xA000_0003))
                        || (accepted.reparse_tag == Some(0xA000_0003)
                            && accepted.source_kind != crate::directory::EntryKind::Folder)
                })
        })
        || request.rename_reparse_guard.as_ref().is_some_and(|_| {
            request.kind != OperationKind::Rename
                || request.sources.len() != 1
                || request.destination.is_none()
        })
        || (request.kind == OperationKind::Rename
            && request
                .sources
                .first()
                .zip(request.destination.as_ref())
                .is_some_and(|(source, destination)| {
                    super::rename::is_case_only_rename(
                        source,
                        destination
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default(),
                    ) && request.move_identity_guard.is_none()
                }))
        || (request.kind == OperationKind::Rename
            && request.move_identity_guard.is_none()
            && request.sources.first().is_some_and(|source| {
                !is_link_or_reparse_point(source)
                    && source
                        .symlink_metadata()
                        .is_ok_and(|metadata| metadata.is_dir())
            }))
    {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    // 受理時に見たfolder rowと同じdirectory treeへ、path-basedなCopyを固定する。
    // conflict wait中もleaseを保持し、request retryでは同じidentityを再照合する。
    #[cfg(windows)]
    let _copy_destination_identity_lease =
        if let Some(expected_identity) = request.copy_destination_identity_guard.as_ref() {
            let Some(path) = destination.as_deref() else {
                emit(ExecutorFact::Terminal {
                    token,
                    report: rejected(),
                });
                return;
            };
            match super::rename::pin_destination_directory_identity(path, expected_identity) {
                Ok(lease) => Some(lease),
                Err(_) => {
                    emit(ExecutorFact::Terminal {
                        token,
                        report: rejected(),
                    });
                    return;
                }
            }
        } else {
            None
        };
    #[cfg(not(windows))]
    if request.copy_destination_identity_guard.is_some() {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    // Link作成はdirect/brokerのどちらでもpathを渡すため、captured parentをworker全体でpinする。
    #[cfg(windows)]
    let _symbolic_link_parent_identity_lease = if request.kind == OperationKind::CreateSymbolicLink
    {
        match request.create_parent_directory_identity_guard.as_ref() {
            Some(expected_identity) => match destination.as_deref().and_then(|path| {
                super::rename::pin_destination_directory_identity(path, expected_identity).ok()
            }) {
                Some(lease) => Some(lease),
                None => {
                    emit(ExecutorFact::Terminal {
                        token,
                        report: rejected(),
                    });
                    return;
                }
            },
            None => None,
        }
    } else {
        None
    };
    #[cfg(not(windows))]
    if request.kind == OperationKind::CreateSymbolicLink
        && request.create_parent_directory_identity_guard.is_some()
    {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    #[cfg(windows)]
    let _move_destination_identity_lease = if request.kind == OperationKind::Move {
        let expected_identity = request
            .move_identity_guard
            .as_ref()
            .map(|guard| &guard.destination_identity)
            .or(request.move_destination_identity_guard.as_ref());
        match expected_identity {
            Some(expected_identity) => match destination.as_deref().and_then(|path| {
                super::rename::pin_destination_directory_identity(path, expected_identity).ok()
            }) {
                Some(lease) => Some(lease),
                None => {
                    emit(ExecutorFact::Terminal {
                        token,
                        report: rejected(),
                    });
                    return;
                }
            },
            None => None,
        }
    } else {
        None
    };
    #[cfg(windows)]
    let _rename_parent_identity_lease = if request.kind == OperationKind::Rename {
        request
            .move_identity_guard
            .as_ref()
            .map(|guard| {
                destination
                    .as_deref()
                    .and_then(std::path::Path::parent)
                    .and_then(|parent| {
                        super::rename::pin_destination_directory_identity(
                            parent,
                            &guard.destination_identity,
                        )
                        .ok()
                    })
            })
            .flatten()
    } else {
        None
    };
    #[cfg(windows)]
    if request.kind == OperationKind::Rename
        && request.move_identity_guard.is_some()
        && _rename_parent_identity_lease.is_none()
    {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    #[cfg(not(windows))]
    if (request.kind == OperationKind::Rename && request.move_identity_guard.is_some())
        || request.move_destination_identity_guard.is_some()
    {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    if request.kind == OperationKind::Move
        && let Some(guard) = request.move_identity_guard.as_mut()
        && guard.source_identities.is_empty()
    {
        let Some(resolved) =
            preflight_local_move_guard(&request.sources, &guard.destination_identity)
        else {
            emit(ExecutorFact::Terminal {
                token,
                report: rejected(),
            });
            return;
        };
        request.move_identity_guard = Some(resolved.clone());
        emit(ExecutorFact::MoveGuardResolved {
            token,
            guard: resolved,
        });
    }
    let has_retry_items = !retry_items.is_empty();
    let initial_attempt = !has_retry_items;
    let items: Vec<_> = if initial_attempt {
        (0..request.sources.len())
            .filter_map(|index| u32::try_from(index).ok())
            .map(ItemId)
            .collect()
    } else {
        retry_items
    };
    // 後続itemが開始前に止まってもmixed requestを識別できるよう、worker上で全sourceを先に分類する。
    // これはfile-only Undoを抑止する印だけで、Undo権限は各adapterのexact receiptからのみ得る。
    if request.kind == OperationKind::Copy && request.virtual_source.is_none() && initial_attempt {
        for (index, source) in request.sources.iter().enumerate() {
            if let Ok(item_index) = u32::try_from(index) {
                let kind = if is_link_or_reparse_point(source) {
                    OrdinaryCopySourceKind::Indeterminate
                } else {
                    match source.symlink_metadata() {
                        Ok(metadata) if metadata.is_dir() => OrdinaryCopySourceKind::Directory,
                        Ok(metadata) if metadata.is_file() => OrdinaryCopySourceKind::File,
                        Ok(_) | Err(_) => OrdinaryCopySourceKind::Indeterminate,
                    }
                };
                emit(ExecutorFact::OrdinaryCopySourceKind { token, item: ItemId(item_index), kind });
            }
        }
    }
    let progress_sources: Vec<_> = items
        .iter()
        .filter_map(|item| request.sources.get(item.0 as usize).cloned())
        .collect();
    let (discovered_items, discovered_bytes) = known_regular_file_facts(&progress_sources);
    let mut counters = ProgressCounters {
        // Folder descendants are discovered while copying rather than holding
        // the first byte behind a recursive pre-scan. Their count is open-ended.
        total_items: match request.kind {
            OperationKind::Copy => known_regular_file_count(&progress_sources),
            _ => Some(items.len() as u64),
        },
        // Directory trees are discovered while copying so that the first byte
        // does not wait for a full recursive pre-scan.  A single regular-file
        // request can provide an exact byte total immediately.
        total_bytes: known_regular_file_bytes(&progress_sources),
        discovered_items,
        discovered_bytes,
        ..Default::default()
    };
    let mut sequence: u64 = 0;
    let mut receipts: Vec<ItemReceipt> = Vec::with_capacity(items.len());
    let mut exact_directory_replace_no_effect = false;
    // This worker invocation is exactly one attempt.  The policy therefore
    // cannot survive cancellation, terminal completion, retry, or another job.
    let recovery_copy = request.kind == OperationKind::Copy
        && request.move_identity_guard.is_some()
        && request.copy_destination_identity_guard.is_some()
        && request.initial_conflict_policy == Some(super::model::ConflictDecision::KeepBoth);
    let mut conflict_policy: Option<super::model::ConflictDecision> = request.initial_conflict_policy;
    // Recovery admission validates and pins the complete ordered source set before
    // the first item can mutate the destination. The pins remain live through the
    // whole worker attempt, including conflicts that appear after this preflight.
    #[cfg(windows)]
    let _recovery_source_pins = if recovery_copy {
        let guard = request.move_identity_guard.as_ref().expect("validated recovery Copy guard");
        let mut pins = Vec::with_capacity(request.sources.len());
        let mut valid = guard.source_identities.len() == request.sources.len();
        for (source, expected) in request.sources.iter().zip(&guard.source_identities) {
            if !valid { break; }
            let Some(metadata) = std::fs::symlink_metadata(source).ok() else { valid = false; break; };
            let kind = if metadata.file_type().is_dir() {
                crate::directory::EntryKind::Folder
            } else {
                crate::directory::EntryKind::File
            };
            let Some(observed) = crate::directory::EntrySourceIdentity::from_path_no_follow(source, kind) else {
                valid = false;
                break;
            };
            if observed.filesystem_identity() != expected.clone() { valid = false; break; }
            let Ok(pin) = windows::preflight_recovery_copy_source(source, expected, cancelled) else {
                valid = false;
                break;
            };
            pins.push(pin);
        }
        if !valid || pins.len() != request.sources.len() {
            emit(ExecutorFact::Terminal { token, report: rejected() });
            return;
        }
        Some(pins)
    } else {
        None
    };
    #[cfg(not(windows))]
    if recovery_copy {
        emit(ExecutorFact::Terminal { token, report: rejected() });
        return;
    }
    let guarded_directory_retry = request.copy_destination_identity_guard.is_some()
        && !copy_directory_retry_guards.is_empty();
    let guarded_file_retry = request.copy_destination_identity_guard.is_some()
        && !copy_retry_guards.is_empty();
    if request.copy_destination_identity_guard.is_some() && has_retry_items
        && !(guarded_directory_retry || guarded_file_retry)
    {
        emit(ExecutorFact::Terminal { token, report: rejected() });
        return;
    }
    if request.copy_destination_identity_guard.is_some() && !has_retry_items
        && (guarded_directory_retry || guarded_file_retry)
    {
        emit(ExecutorFact::Terminal { token, report: rejected() });
        return;
    }
    if !copy_directory_retry_guards.is_empty()
        && (request.kind != OperationKind::Copy
            || request.virtual_source.is_some()
            || request.initial_conflict_policy.is_some()
            || request.move_identity_guard.is_some()
            || request.move_destination_identity_guard.is_some()
            || request.move_replace_source_evidence.is_some()
            || request.rename_reparse_guard.is_some()
            || request.source_cleanup_identity_guard.is_some()
            || request.batch_rename_plan.is_some()
            || (request.copy_destination_identity_guard.is_some() && !guarded_directory_retry))
    {
        emit(ExecutorFact::Terminal { token, report: rejected() });
        return;
    }
    if guarded_directory_retry || guarded_file_retry {
        let Some(destination_identity_guard) = request.copy_destination_identity_guard.as_ref() else {
            unreachable!("guarded Copy retry was derived from this request");
        };
        let retry_set = items.iter().copied().collect::<std::collections::HashSet<_>>();
        let mut file_items = std::collections::HashSet::new();
        let mut directory_items = std::collections::HashSet::new();
        let exact_file_guards = copy_retry_guards.iter().all(|guard| {
                let Some(source) = request.sources.get(guard.item.0 as usize) else { return false; };
                let Some(destination) = request.destination.as_deref().zip(source.file_name())
                    .map(|(parent, name)| parent.join(name)) else { return false; };
                file_items.insert(guard.item) && retry_set.contains(&guard.item)
                    && guard.source == *source && guard.destination == destination
                    && guard.destination_parent_identity == *destination_identity_guard
                    && guard.source_identity.windows_parts().is_some()
                    && guard.destination_parent_identity.windows_parts().is_some()
            });
        let exact_directory_guards = copy_directory_retry_guards.iter().all(|guard| {
                let Some(source) = request.sources.get(guard.item.0 as usize) else { return false; };
                let Some(destination) = request.destination.as_deref().zip(source.file_name())
                    .map(|(parent, name)| parent.join(name)) else { return false; };
                directory_items.insert(guard.item)
                    && retry_set.contains(&guard.item)
                    && guard.source == *source
                    && guard.destination == destination
                    && guard.destination_parent_identity == *destination_identity_guard
                    && guard.source_root_identity.windows_parts().is_some()
                    && guard.destination_parent_identity.windows_parts().is_some()
            });
        let exact_guards = retry_set.len() == items.len()
            && retry_set.len() == file_items.len() + directory_items.len()
            && file_items.is_disjoint(&directory_items)
            && file_items.union(&directory_items).copied().collect::<std::collections::HashSet<_>>() == retry_set
            && exact_file_guards && exact_directory_guards;
        if !exact_guards {
            emit(ExecutorFact::Terminal { token, report: rejected() });
            return;
        }
    }
    if guarded_file_retry || guarded_directory_retry {
        let Some(destination_identity_guard) = request.copy_destination_identity_guard.as_ref() else {
            unreachable!("guarded file retry was derived from this request");
        };
        let retry_set = items.iter().copied().collect::<std::collections::HashSet<_>>();
        let mut guard_items = std::collections::HashSet::new();
        let exact_guards = request.kind == OperationKind::Copy
            && request.virtual_source.is_none()
            && request.initial_conflict_policy.is_none()
            && request.move_identity_guard.is_none()
            && request.move_destination_identity_guard.is_none()
            && request.move_replace_source_evidence.is_none()
            && request.rename_reparse_guard.is_none()
            && request.source_cleanup_identity_guard.is_none()
            && request.batch_rename_plan.is_none()
            && retry_set.len() == items.len()
            && (copy_retry_guards.is_empty() || copy_retry_guards.iter().all(|guard| {
                let Some(source) = request.sources.get(guard.item.0 as usize) else { return false; };
                let Some(destination) = request.destination.as_deref().zip(source.file_name())
                    .map(|(parent, name)| parent.join(name)) else { return false; };
                guard_items.insert(guard.item)
                    && retry_set.contains(&guard.item)
                    && guard.source == *source
                    && guard.destination == destination
                    && guard.destination_parent_identity == *destination_identity_guard
                    && guard.source_identity.windows_parts().is_some()
                    && guard.destination_parent_identity.windows_parts().is_some()
            }))
            && guard_items.iter().all(|item| retry_set.contains(item));
        if !exact_guards {
            emit(ExecutorFact::Terminal { token, report: rejected() });
            return;
        }
    }
    let mut pending_items = items.into_iter();
    while let Some(item) = pending_items.next() {
        if matches!(
            request.kind,
            OperationKind::BatchRename | OperationKind::BatchRenameUndo
        ) && receipts
            .iter()
            .any(|receipt| receipt.outcome != ItemOutcome::Completed)
        {
            for suffix in std::iter::once(item).chain(pending_items) {
                receipts.push(ItemReceipt {
                    item: suffix,
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: Some(
                        "cancelled because an earlier batch rename item did not complete".into(),
                    ),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                });
            }
            break;
        }
        let Some(source) = request.sources.get(item.0 as usize) else {
            receipts.push(ItemReceipt {
                item,
                outcome: ItemOutcome::Failed,
                detail: Some("invalid retry item".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            continue;
        };
        #[cfg(windows)]
        if let Some(expected) = request.move_replace_source_evidence.as_ref()
            .and_then(|proofs| proofs.get(item.0 as usize))
        {
            let source_matches = windows::prepare_regular_file_move_source(source).ok()
                .and_then(|opened| opened.shared_identity()) == Some(expected.source_identity.clone());
            if !source_matches {
                receipts.push(ItemReceipt { item, outcome: ItemOutcome::Failed,
                    detail: Some("clipboard Move source identity changed before mutation".into()),
                    created_folder_path: None, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None });
                continue;
            }
        }
        let trash_tag = if request.kind == OperationKind::Trash {
            let Some(guard) = request
                .trash_source_guards
                .as_ref()
                .and_then(|guards| guards.get(item.0 as usize))
            else {
                emit(ExecutorFact::Terminal {
                    token,
                    report: rejected(),
                });
                return;
            };
            match super::trash::admit_source(source, guard) {
                Ok(tag) => tag,
                Err(_) => {
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Failed,
                        detail: Some(
                            "Trash source identity changed before adapter dispatch".into(),
                        ),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
            }
        } else {
            None
        };
        let clipboard_move_source = request.clipboard_move_source_evidence
            .as_ref()
            .and_then(|evidence| evidence.get(item.0 as usize));
        let move_guard = (request.kind == OperationKind::Move)
            .then(|| request.move_identity_guard.as_ref())
            .flatten()
            .map(|guard| (
                &guard.source_identities[item.0 as usize],
                &guard.destination_identity,
            ));
        let copy_retry_guard = copy_retry_guards.iter().find(|guard| guard.item == item);
        let copy_directory_retry_guard = copy_directory_retry_guards.iter().find(|guard| guard.item == item);
        let cross_volume_move_retry_guard = cross_volume_move_retry_guards
            .iter()
            .find(|guard| guard.proof.item == item);
        if request.kind == OperationKind::Copy
            && (copy_retry_guard.is_some() || copy_directory_retry_guard.is_some())
            && is_link_or_reparse_point(source)
        {
            receipts.push(ItemReceipt {
                item,
                outcome: ItemOutcome::Failed,
                detail: Some("guarded Copy retry source became a link or reparse point".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            continue;
        }
        if move_guard.is_some() && is_link_or_reparse_point(source) {
            receipts.push(ItemReceipt {
                item,
                outcome: ItemOutcome::Failed,
                detail: Some("guarded move does not accept reparse points".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            continue;
        }
        // UI の selection は列挙時の link fact で先に capability gate するが、CF_HDROP や
        // stale clipboard など view を通らない source もここへ来る。各 operation の
        // no-follow adapter が明示的に受け付けない reparse point は、通常 file/folder と
        // 誤認させずここで止める。
        let batch_reparse_source_is_authorized = matches!(
            request.kind,
            OperationKind::BatchRename | OperationKind::BatchRenameUndo
        ) && request
            .batch_rename_plan
            .as_ref()
            .and_then(|plan| plan.get(item.0 as usize))
            .is_some_and(|target| {
                target.source == *source
                    && target.source_identity.windows_parts().is_some()
                    && target.parent_identity.windows_parts().is_some()
                    && target.reparse_guard.as_ref().is_some_and(|guard| {
                        guard.source_identity == target.source_identity
                            && guard.parent_identity == target.parent_identity
                            && matches!(guard.reparse_tag, 0xA000_000C | 0xA000_0003)
                            && (guard.reparse_tag != 0xA000_0003
                                || target.kind == crate::directory::EntryKind::Folder)
                    })
            });
        if !matches!(
            request.kind,
            OperationKind::Copy
                | OperationKind::Move
                | OperationKind::Trash
                | OperationKind::CreateSymbolicLink
                | OperationKind::CreateJunction
        ) && is_link_or_reparse_point(source)
            && !batch_reparse_source_is_authorized
        {
            receipts.push(ItemReceipt {
                item,
                outcome: ItemOutcome::Failed,
                detail: Some("links and junctions are not supported yet".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            continue;
        }
        // 複合台帳の障害境界と電源断検証が済むまで通常経路では無効にする。
        let directory_move_replace_enabled = options.directory_move_replace_enabled;
        let directory_move_replace_rename_enabled = options.directory_move_replace_rename_enabled;
        // Retry guardは確認時の通常fileに結び付いている。worker開始前にsource pathが
        // directoryへ差し替わっても、directory用の別protocolへ再分類しない。
        let retry_guarded_regular_file =
            copy_retry_guard.is_some() || cross_volume_move_retry_guard.is_some();
        let source_is_directory = matches!(request.kind, OperationKind::Copy | OperationKind::Move)
            && !retry_guarded_regular_file
            && clipboard_move_source.map_or_else(
                || !is_link_or_reparse_point(source)
                    && (copy_directory_retry_guard.is_some()
                        || source.symlink_metadata().is_ok_and(|metadata| metadata.is_dir())),
                |accepted| accepted.reparse_tag.is_none()
                    && accepted.source_kind == crate::directory::EntryKind::Folder,
            );
        #[cfg(windows)]
        let directory_move_identities = if request.kind == OperationKind::Move
            && cross_volume_move_retry_guard.is_none()
            && source_is_directory
        {
            let source_identity = move_guard
                .map(|(identity, _)| identity.clone())
                .or_else(|| {
                    crate::directory::EntrySourceIdentity::from_path(
                        source,
                        crate::directory::EntryKind::Folder,
                    )
                    .map(|identity| identity.filesystem_identity())
                });
            let destination_identity = move_guard
                .map(|(_, identity)| identity.clone())
                .or_else(|| request.move_destination_identity_guard.clone())
                .or_else(|| {
                    destination.as_deref().and_then(|path| {
                        crate::directory::EntrySourceIdentity::from_path(
                            path,
                            crate::directory::EntryKind::Folder,
                        )
                        .map(|identity| identity.filesystem_identity())
                    })
                });
            source_identity.zip(destination_identity)
        } else {
            None
        };
        #[cfg(windows)]
        let directory_move_cross_volume = request.kind == OperationKind::Move
            && !is_link_or_reparse_point(source)
            && directory_move_identities.as_ref().is_some_and(
                |(source_identity, destination_identity)| {
                    source_identity
                        .windows_parts()
                        .zip(destination_identity.windows_parts())
                        .is_some_and(|((source_volume, _), (destination_volume, _))| {
                            source_volume != destination_volume
                        })
                },
            );
        #[cfg(not(windows))]
        let directory_move_cross_volume = false;
        // Rename captures its fully-qualified target at command acceptance;
        // copy/move instead use a destination directory and preserve the name.
        let destination_path = if matches!(
            request.kind,
            OperationKind::BatchRename | OperationKind::BatchRenameUndo
        ) {
            request
                .batch_rename_plan
                .as_ref()
                .and_then(|plan| plan.get(item.0 as usize))
                .map(|target| target.destination.clone())
        } else {
            destination.as_ref().map(|destination| match request.kind {
                OperationKind::Rename => destination.clone(),
                _ => destination.join(source.file_name().unwrap_or_default()),
            })
        };
        // Batch Rename owns immutable destinations and reports any collision
        // through its ordered per-item failure/stop protocol. It must not enter
        // the generic Copy/Move conflict UI, which can offer name synthesis.
        if !matches!(
            request.kind,
            OperationKind::Rename
                | OperationKind::BatchRename
                | OperationKind::BatchRenameUndo
        )
            && copy_retry_guard.is_none()
            && copy_directory_retry_guard.is_none()
            && cross_volume_move_retry_guard.is_none()
            && destination_path.as_ref().is_some_and(|path| path.exists())
        {
            let destination_path = destination_path
                .as_ref()
                .expect("copy/move has destination");
            #[cfg(target_os = "windows")]
            let replace_target = matches!(request.kind, OperationKind::Copy | OperationKind::Move)
                .then(|| windows::prepare_regular_file_replace(destination_path))
                .and_then(Result::ok);
            #[cfg(target_os = "windows")]
            let replace_source = if request.kind == OperationKind::Copy {
                windows::prepare_regular_file_source(source).ok()
            } else if request.kind == OperationKind::Move {
                (request.clipboard_move_source_evidence.is_none()
                    .then(|| move_guard.map(|(expected_source, _)| expected_source))
                    .flatten())
                    .or_else(|| request.move_replace_source_evidence.as_ref()
                        .and_then(|evidence| evidence.get(item.0 as usize))
                        .map(|evidence| &evidence.source_identity))
                    .and_then(|expected_source| {
                        windows::prepare_regular_file_move_source(source)
                            .ok()
                            .filter(|opened| {
                                opened.shared_identity().as_ref() == Some(expected_source)
                            })
                    })
            } else {
                None
            };
            #[cfg(target_os = "windows")]
            let mut replace_folder_source = if (request.kind == OperationKind::Copy
                || (request.kind == OperationKind::Move
                    && ((directory_move_cross_volume && directory_move_replace_enabled)
                        || (!directory_move_cross_volume
                            && directory_move_replace_rename_enabled))))
                && source_is_directory
                && request.sources.len() == 1
            {
                if request.kind == OperationKind::Move
                    && !directory_move_cross_volume
                    && directory_move_replace_rename_enabled
                {
                    destination_path.parent()
                        .and_then(|parent| windows::preflight_pinned_source_tree_for_directory_replace_rename(source, parent, cancelled).ok())
                } else {
                    windows::preflight_pinned_source_tree(source, cancelled).ok()
                }
            } else {
                None
            };
            #[cfg(target_os = "windows")]
            let mut replace_folder_target = if (request.kind == OperationKind::Copy
                || (request.kind == OperationKind::Move
                    && ((directory_move_cross_volume && directory_move_replace_enabled)
                        || (!directory_move_cross_volume
                            && directory_move_replace_rename_enabled))))
                && source_is_directory
                && request.sources.len() == 1
            {
                if request.kind == OperationKind::Move
                    && !directory_move_cross_volume
                    && directory_move_replace_rename_enabled
                {
                    destination_path.parent()
                        .and_then(|parent| windows::preflight_pinned_source_tree_for_directory_replace_rename(destination_path, parent, cancelled).ok())
                } else {
                    windows::preflight_pinned_source_tree(destination_path, cancelled).ok()
                }
            } else {
                None
            };
            #[cfg(target_os = "windows")]
            let replace_target = if request.kind == OperationKind::Move {
                replace_target.filter(|target| {
                    replace_source.as_ref().is_some_and(|source| {
                        !source.same_object_as(target)
                            && (source.same_volume_as(target)
                                // cross-volume Replace のバッチ回復境界を閉じるまでは singleton に限定する。
                                || (options.cross_volume_regular_file_move_replace_enabled
                                    && request.sources.len() == 1))
                    })
                })
            } else {
                replace_target
            };
            let directory_move_replace_rename_candidate = if directory_move_replace_rename_enabled
                && request.kind == OperationKind::Move
                && !directory_move_cross_volume
                && source_is_directory
                && request.sources.len() == 1
            {
                #[cfg(windows)]
                {
                    replace_folder_source
                        .as_ref()
                        .zip(replace_folder_target.as_ref())
                        .and_then(|(source_tree, target_tree)| {
                            windows::directory_move_replace_rename_candidate(
                                source,
                                destination_path,
                                source_tree,
                                target_tree,
                            )
                            .ok()
                        })
                }
                #[cfg(not(windows))]
                { None }
            } else {
                None
            };
            let notice = super::model::ConflictNotice {
                item,
                probe_revision: sequence.wrapping_add(1),
                description: format!("{} already exists", destination_path.display()),
                directory_move_replace_rename_candidate: directory_move_replace_rename_candidate.clone(),
                #[cfg(target_os = "windows")]
                replace_supported: (replace_target.is_some() && replace_source.is_some())
                    || (request.kind == OperationKind::Copy
                        && source_is_directory
                        && request.sources.len() == 1
                        && replace_folder_source.is_some()
                        && replace_folder_target.is_some())
                    || (directory_move_replace_enabled
                        && directory_move_cross_volume
                        && source_is_directory
                        && request.sources.len() == 1
                        && replace_folder_source.is_some()
                        && replace_folder_target.is_some())
                    || directory_move_replace_rename_candidate.is_some(),
                #[cfg(not(target_os = "windows"))]
                replace_supported: false,
            };
            let directory_move_replace_candidate =
                notice.directory_move_replace_rename_candidate.clone();
            let automatic = conflict_policy.filter(|decision| {
                *decision != super::model::ConflictDecision::Replace || notice.replace_supported
            });
            if automatic.is_none() {
                emit(ExecutorFact::Conflict { token, notice });
            }
            let response = automatic.map_or_else(
                || controls.recv(),
                |decision| {
                    Ok(WorkerControl::Resolve {
                        decision,
                        apply_to_remaining: false,
                    })
                },
            );
            match response {
                Ok(WorkerControl::DirectoryMoveReplaceRenameApproved {
                    token: approved_token,
                    candidate,
                }) => {
                    let exact_approval = approved_token.attempt_token() == token
                        && approved_token.item_id() == item
                        && directory_move_replace_candidate.as_ref() == Some(&candidate);
                    #[cfg(windows)]
                    if exact_approval && options.directory_move_replace_rename_enabled {
                        let mut emit_fact = |fact| emit(fact);
                        let displacement = replace_folder_source
                            .take()
                            .zip(replace_folder_target.take())
                            .ok_or(CopyError::Unsupported)
                            .and_then(|(source_tree, target_tree)| {
                                windows::displace_approved_directory_move_replace_target(
                                    token,
                                    approved_token,
                                    &candidate,
                                    source_tree,
                                    target_tree,
                                    cancelled,
                                    controls,
                                    &mut emit_fact,
                                    #[cfg(test)] options.directory_move_replace_rename_test_hook.take(),
                                )
                            });
                        let (outcome, detail) = match displacement {
                            Ok(_) => (ItemOutcome::Completed, None),
                            Err(CopyError::DirectoryMoveReplaceNoEffect { cancelled: was_cancelled }) => {
                                exact_directory_replace_no_effect = true;
                                if was_cancelled {
                                    (ItemOutcome::CancelledBeforeStart, Some("cancelled after durable NoEffect confirmation".into()))
                                } else {
                                    (ItemOutcome::Failed, Some("target displacement was refused before mutation; durable NoEffect confirmed".into()))
                                }
                            }
                            Err(CopyError::Cancelled) => (ItemOutcome::CancelledBeforeStart, Some("cancelled before target displacement completed".into())),
                            Err(CopyError::Unknown(detail) | CopyError::Partial(detail)) => (ItemOutcome::Partial, Some(detail)),
                            Err(error) => (ItemOutcome::Failed, Some(format!("same-volume directory Replace stopped before proven displacement: {error:?}"))),
                        };
                        receipts.push(ItemReceipt {
                            item,
                            outcome,
                            detail,
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        });
                        continue;
                    }
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Failed,
                        detail: Some(if exact_approval {
                            "same-volume directory Replace remains gated".into()
                        } else {
                            "mismatched same-volume directory Replace approval".into()
                        }),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                Ok(WorkerControl::Resolve {
                    decision: super::model::ConflictDecision::Skip,
                    apply_to_remaining,
                }) => {
                    if apply_to_remaining {
                        conflict_policy = Some(super::model::ConflictDecision::Skip);
                    }
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Skipped,
                        detail: None,
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                Ok(WorkerControl::Resolve {
                    decision: super::model::ConflictDecision::KeepBoth,
                    apply_to_remaining,
                }) => {
                    if apply_to_remaining {
                        conflict_policy = Some(super::model::ConflictDecision::KeepBoth);
                    }
                    // Replace用に取得したDELETE非共有handleは、Keep Both側の
                    // guarded Moveが同じsourceを開く前に解放する。
                    #[cfg(windows)]
                    if request.kind == OperationKind::Move {
                        drop(replace_source);
                        drop(replace_target);
                    }
                    let mut resolved_destination = destination_path.clone();
                    let mut copy_undo = None;
                    let mut copy_completion_progress_counted = false;
                    let result = match request.kind {
                        OperationKind::Copy => {
                            let progress = RefCell::new(
                                |bytes, completed_files, discovered: Option<(u64, u64)>| {
                                    counters.processed_bytes =
                                        counters.processed_bytes.saturating_add(bytes);
                                    copy_completion_progress_counted |= completed_files > 0;
                                    counters.processed_items =
                                        counters.processed_items.saturating_add(completed_files);
                                    if let Some((items, bytes)) = discovered {
                                        counters.discovered_items =
                                            counters.discovered_items.saturating_add(items);
                                        counters.discovered_bytes =
                                            counters.discovered_bytes.saturating_add(bytes);
                                    }
                                    sequence += 1;
                                    emit(ExecutorFact::Progress {
                                        token,
                                        update: ProgressUpdate {
                                            sequence,
                                            counters,
                                            current_item: Some(item),
                                        },
                                    });
                                },
                            );
                            #[cfg(windows)]
                            if !source_is_directory
                                && request.virtual_source.is_none()
                                && copy_retry_guard.is_none()
                                && copy_directory_retry_guard.is_none()
                                && !is_link_or_reparse_point(source)
                            {
                                let mut failures = Vec::new();
                                let mut custody = Vec::new();
                                let result = copy_keep_both_regular_file(
                                    source,
                                    destination_path,
                                    token,
                                    item,
                                    cancelled,
                                    &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                    &mut |provenance| failures.push(provenance),
                                    &mut |stage| custody.push(stage),
                                );
                                if result.is_ok() {
                                    (progress.borrow_mut())(0, 1, None);
                                }
                                drop(progress);
                                for provenance in failures {
                                    emit(ExecutorFact::CopyFailureProvenance { token, provenance });
                                }
                                for custody in custody {
                                    emit(ExecutorFact::CopyStageCustody {
                                        custody: super::runtime::CopyStageCustodyTransfer::new(custody),
                                    });
                                }
                                match result {
                                    Ok(receipt) => {
                                        copy_undo = Some(receipt);
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            } else if source_is_directory
                                && request.virtual_source.is_none()
                                && copy_retry_guard.is_none()
                                && !is_link_or_reparse_point(source)
                            {
                                #[cfg(windows)]
                                {
                                    let mut receipt = None;
                                    let mut recovery_candidate = destination_path.clone();
                                    let result = if recovery_copy {
                                        copy_keep_both_directory_recovery(
                                            source,
                                            destination_path,
                                            token,
                                            item,
                                            cancelled,
                                            &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                            &mut || (progress.borrow_mut())(0, 1, None),
                                            &mut |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                                            &mut |proof| receipt = Some(proof),
                                            &mut recovery_candidate,
                                        )
                                    } else {
                                        copy_keep_both_directory(
                                            source,
                                            destination_path,
                                            token,
                                            item,
                                            cancelled,
                                            &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                            &mut || (progress.borrow_mut())(0, 1, None),
                                            &mut |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                                            &mut |proof| receipt = Some(proof),
                                        )
                                    };
                                    if recovery_copy && result.is_ok() {
                                        resolved_destination = recovery_candidate;
                                    }
                                    if result.is_ok() {
                                        if let Some(receipt) = receipt {
                                            emit(ExecutorFact::DirectoryCopyUndoCandidate { token, item, receipt });
                                        }
                                    }
                                    result
                                }
                                #[cfg(not(windows))]
                                { Err(CopyError::Unsupported) }
                            } else {
                                copy_keep_both(
                                    source,
                                    destination_path,
                                    token,
                                    item,
                                    cancelled,
                                    |bytes| (progress.borrow_mut())(bytes, 0, None),
                                    || (progress.borrow_mut())(0, 1, None),
                                    |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                                )
                            }
                            #[cfg(not(windows))]
                            copy_keep_both(
                                source,
                                destination_path,
                                token,
                                item,
                                cancelled,
                                |bytes| (progress.borrow_mut())(bytes, 0, None),
                                || (progress.borrow_mut())(0, 1, None),
                                |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                            )
                        }
                        OperationKind::Move => {
                            let on_destination =
                                |candidate: &Path| resolved_destination = candidate.to_path_buf();
                            #[cfg(windows)]
                            if directory_move_cross_volume {
                                let (source_identity, destination_identity) =
                                    directory_move_identities
                                        .as_ref()
                                        .expect("cross-volume directory Move identities");
                                let event_sink = std::cell::RefCell::new(&mut emit);
                                let mut on_bytes = |bytes| {
                                    counters.processed_bytes =
                                        counters.processed_bytes.saturating_add(bytes);
                                    sequence += 1;
                                    (event_sink.borrow_mut())(ExecutorFact::Progress {
                                        token,
                                        update: ProgressUpdate {
                                            sequence,
                                            counters,
                                            current_item: Some(item),
                                        },
                                    });
                                };
                                let mut emit_fact = |fact| (event_sink.borrow_mut())(fact);
                                move_keep_both_cross_volume_directory(
                                    token,
                                    item,
                                    source,
                                    destination_path,
                                    source_identity,
                                    destination_identity,
                                    cancelled,
                                    controls,
                                    on_destination,
                                    &mut on_bytes,
                                    &mut emit_fact,
                                    request.move_destination_identity_guard.is_some()
                                        && request.move_identity_guard.is_none(),
                                )
                            } else if let Some((source_identity, destination_identity)) = move_guard
                            {
                                move_keep_both_guarded(
                                    source,
                                    destination_path,
                                    source_identity,
                                    destination_identity,
                                    cancelled,
                                    on_destination,
                                    &mut |receipt| {
                                        if request.move_destination_identity_guard.is_some()
                                            && request.move_identity_guard.is_none()
                                        {
                                            emit(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                                        }
                                    },
                                )
                            } else {
                                let event_sink = std::cell::RefCell::new(&mut emit);
                                let mut on_bytes = |bytes| {
                                    counters.processed_bytes = counters.processed_bytes.saturating_add(bytes);
                                    sequence += 1;
                                    (event_sink.borrow_mut())(ExecutorFact::Progress {
                                        token,
                                        update: ProgressUpdate { sequence, counters, current_item: Some(item) },
                                    });
                                };
                                let mut on_source_retained = |proof| {
                                    (event_sink.borrow_mut())(ExecutorFact::PublishedMoveSourceRetained { token, proof });
                                };
                                let mut on_disposition = |proof| {
                                    (event_sink.borrow_mut())(ExecutorFact::PublishedMoveDisposition { token, proof });
                                };
                                let mut on_no_effect = |proof| {
                                    (event_sink.borrow_mut())(ExecutorFact::CrossVolumeMoveNoEffectProof { token, proof });
                                };
                                move_keep_both_observed(
                                    source,
                                    destination_path,
                                    cancelled,
                                    on_destination,
                                    &mut on_bytes,
                                    item,
                                    &mut on_source_retained,
                    &mut on_disposition,
                    &mut on_no_effect,
                    request.move_destination_identity_guard.as_ref(),
                    clipboard_move_source,
                    &mut |receipt| {
                                        if request.move_destination_identity_guard.is_some()
                                            && request.move_identity_guard.is_none()
                                        {
                                            (event_sink.borrow_mut())(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                                        }
                                    },
                                )
                            }
                        }
                        OperationKind::CreateSymbolicLink => create_symbolic_link_keep_both(
                            source,
                            destination_path,
                            request.create_parent_directory_identity_guard.as_ref(),
                            cancelled,
                            |candidate| resolved_destination = candidate.to_path_buf(),
                        ),
                        OperationKind::CreateJunction => create_junction_keep_both(
                            source,
                            destination_path,
                            cancelled,
                            |candidate| resolved_destination = candidate.to_path_buf(),
                        ),
                        _ => unreachable!(),
                    };
                    let destination_published_override =
                        matches!(&result, Err(CopyError::PrePublishPartial(_))).then_some(false);
                    let (outcome, detail) = copy_outcome(result);
                    if outcome == ItemOutcome::Completed
                        && (!source_is_directory || request.kind == OperationKind::Move)
                        && !(request.kind == OperationKind::Copy
                            && copy_completion_progress_counted)
                    {
                        counters.processed_items += 1;
                    }
                    let mut receipt = item_receipt(
                        request.kind,
                        item,
                        source,
                        Some(&resolved_destination),
                        outcome,
                        detail,
                        destination_published_override,
                        None,
                    );
                    receipt.copy_undo = copy_undo;
                    receipts.push(receipt);
                    #[cfg(windows)]
                    if directory_move_cross_volume && outcome != ItemOutcome::Completed {
                        for suffix in pending_items {
                            receipts.push(ItemReceipt {
                                item: suffix,
                                outcome: ItemOutcome::CancelledBeforeStart,
                                detail: Some(
                                    "not started because an earlier directory Move Keep Both requires recovery".into(),
                                ),
                                created_folder_path: None,
                                created_archive_path: None,
                                move_receipt: None,
                                replace_receipt: None, copy_undo: None,
                            });
                        }
                        break;
                    }
                    continue;
                }
                Ok(WorkerControl::Resolve {
                    decision: super::model::ConflictDecision::Replace,
                    apply_to_remaining,
                }) => {
                    if directory_move_replace_candidate.is_some() {
                        receipts.push(ItemReceipt {
                            item,
                            outcome: ItemOutcome::Failed,
                            detail: Some("same-volume directory Replace requires its exact approval control".into()),
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        });
                        continue;
                    }
                    if apply_to_remaining {
                        conflict_policy = Some(super::model::ConflictDecision::Replace);
                    }
                    #[cfg(target_os = "windows")]
                    {
                        if directory_move_replace_enabled
                            && source_is_directory
                            && request.kind == OperationKind::Move
                        {
                            let (Some(source_tree), Some(target_tree)) =
                                (replace_folder_source, replace_folder_target)
                            else {
                                receipts.push(ItemReceipt { item, outcome: ItemOutcome::Failed, detail: Some("directory Move Replace capability changed after conflict presentation".into()), created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None });
                                continue;
                            };
                            let Some((source_identity, destination_parent_identity)) =
                                directory_move_identities.as_ref()
                            else {
                                receipts.push(ItemReceipt { item, outcome: ItemOutcome::Failed, detail: Some("directory Move Replace requires captured source and destination identities".into()), created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None });
                                continue;
                            };
                            let event_sink = RefCell::new(&mut emit);
                            let mut on_bytes = |bytes| {
                                counters.processed_bytes =
                                    counters.processed_bytes.saturating_add(bytes);
                                sequence += 1;
                                (event_sink.borrow_mut())(ExecutorFact::Progress {
                                    token,
                                    update: ProgressUpdate {
                                        sequence,
                                        counters,
                                        current_item: Some(item),
                                    },
                                });
                            };
                            let mut emit_fact = |fact| (event_sink.borrow_mut())(fact);
                            let result = windows::replace_move_cross_volume_directory(
                                token,
                                item,
                                source,
                                destination_path,
                                source_identity,
                                destination_parent_identity,
                                source_tree,
                                target_tree,
                                cancelled,
                                controls,
                                &mut on_bytes,
                                &mut emit_fact,
                            );
                            let (outcome, detail) = copy_outcome(result);
                            if outcome == ItemOutcome::Completed {
                                counters.processed_items += 1;
                            }
                            receipts.push(ItemReceipt {
                                item,
                                outcome,
                                detail,
                                created_folder_path: None,
                                created_archive_path: None,
                                move_receipt: None,
                                replace_receipt: None,
                                copy_undo: None,
                            });
                            if outcome != ItemOutcome::Completed {
                                for pending_item in pending_items {
                                    receipts.push(ItemReceipt { item: pending_item, outcome: ItemOutcome::CancelledBeforeStart, detail: Some("not started because an earlier directory Move Replace requires recovery".into()), created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None });
                                }
                                break;
                            }
                            continue;
                        }
                        if source_is_directory && request.kind == OperationKind::Copy {
                            let (Some(source_tree), Some(target_tree)) =
                                (replace_folder_source, replace_folder_target)
                            else {
                                receipts.push(ItemReceipt { item, outcome: ItemOutcome::Failed, detail: Some("folder Replace capability changed after conflict presentation".into()), created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None });
                                continue;
                            };
                            let folder_emit = RefCell::new(&mut emit);
                            let mut folder_bytes = |bytes| {
                                counters.processed_bytes =
                                    counters.processed_bytes.saturating_add(bytes);
                                sequence += 1;
                                (folder_emit.borrow_mut())(ExecutorFact::Progress {
                                    token,
                                    update: ProgressUpdate {
                                        sequence,
                                        counters,
                                        current_item: Some(item),
                                    },
                                });
                            };
                            let replaced = windows::replace_folder(
                                source,
                                source_tree,
                                target_tree,
                                destination_path,
                                token,
                                item,
                                cancelled,
                                controls,
                                &mut folder_bytes,
                                &mut |fact| (folder_emit.borrow_mut())(fact),
                            );
                            match replaced {
                                Ok(attempt) => {
                                    let complete = attempt.receipt.destination_published
                                        && attempt.receipt.displaced_backup_removed;
                                    let outcome = if complete {
                                        ItemOutcome::Completed
                                    } else {
                                        ItemOutcome::Partial
                                    };
                                    if complete {
                                        counters.processed_items += 1;
                                    }
                                    receipts.push(ItemReceipt {
                                        item,
                                        outcome,
                                        detail: attempt.detail,
                                        created_folder_path: None,
                                        created_archive_path: None,
                                        move_receipt: None,
                                        replace_receipt: Some(attempt.receipt),
                                        copy_undo: None,
                                    });
                                    if !complete {
                                        for pending_item in pending_items {
                                            receipts.push(ItemReceipt { item: pending_item, outcome: ItemOutcome::CancelledBeforeStart, detail: Some("not started because an earlier Copy Replace requires recovery".into()), created_folder_path: None, created_archive_path: None, move_receipt: None, replace_receipt: None, copy_undo: None });
                                        }
                                        break;
                                    }
                                }
                                Err(error) => {
                                    let (outcome, detail) = copy_outcome(Err(error));
                                    receipts.push(ItemReceipt {
                                        item,
                                        outcome,
                                        detail,
                                        created_folder_path: None,
                                        created_archive_path: None,
                                        move_receipt: None,
                                        replace_receipt: None,
                                        copy_undo: None,
                                    });
                                }
                            }
                            continue;
                        }
                        let (Some(source_handle), Some(target)) = (replace_source, replace_target)
                        else {
                            receipts.push(ItemReceipt {
                                item,
                                outcome: ItemOutcome::Failed,
                                detail: Some("replace supports regular files only".into()),
                                created_folder_path: None,
                                created_archive_path: None,
                                move_receipt: None,
                                replace_receipt: None,
                                copy_undo: None,
                            });
                            continue;
                        };
                        let mut replace_copy_undo = None;
                        let result = if request.kind == OperationKind::Move {
                            let expected_parent = move_guard
                                .map(|(_, expected_parent)| expected_parent)
                                .or_else(|| request.move_destination_identity_guard.as_ref());
                            let Some(expected_parent) = expected_parent else {
                                receipts.push(ItemReceipt {
                                    item,
                                    outcome: ItemOutcome::Failed,
                                    detail: Some(
                                        "Move Replace requires a captured destination identity"
                                            .into(),
                                    ),
                                    created_folder_path: None,
                                    created_archive_path: None,
                                    move_receipt: None,
                                    replace_receipt: None,
                                    copy_undo: None,
                                });
                                continue;
                            };
                            windows::replace_move_regular_file(
                                source,
                                source_handle,
                                destination_path,
                                target,
                                expected_parent,
                                token,
                                item,
                                cancelled,
                                controls,
                                &mut |fact| emit(fact),
                            )
                        } else {
                            windows::replace_regular_file(
                                source,
                                source_handle,
                                destination_path,
                                target,
                                token,
                                item,
                                cancelled,
                                controls,
                                &mut |event| match event {
                                    ReplaceEvent::BackupIntent(fact) => emit(fact),
                                    ReplaceEvent::BytesCopied(bytes) => {
                                        counters.processed_bytes += bytes;
                                        sequence += 1;
                                        emit(ExecutorFact::Progress {
                                            token,
                                            update: ProgressUpdate {
                                                sequence,
                                                counters,
                                                current_item: Some(item),
                                            },
                                        });
                                    }
                                },
                                &mut || {},
                                &mut |receipt| {
                                    if request.kind == OperationKind::Copy
                                        && request.virtual_source.is_none()
                                        && !is_link_or_reparse_point(source)
                                        && source
                                            .symlink_metadata()
                                            .is_ok_and(|metadata| metadata.is_file())
                                    {
                                        replace_copy_undo = Some(receipt);
                                    }
                                },
                            )
                        };
                        match result {
                            Ok(attempt) => {
                                let outcome = if attempt.receipt.destination_published
                                    && attempt.receipt.displaced_backup_removed
                                {
                                    ItemOutcome::Completed
                                } else {
                                    ItemOutcome::Partial
                                };
                                if outcome == ItemOutcome::Completed {
                                    counters.processed_items += 1;
                                }
                                receipts.push(ItemReceipt {
                                    item,
                                    outcome,
                                    detail: attempt.detail,
                                    created_folder_path: None,
                                    created_archive_path: None,
                                    move_receipt: None,
                                    replace_receipt: Some(attempt.receipt),
                                    copy_undo: if outcome == ItemOutcome::Completed {
                                        replace_copy_undo
                                    } else {
                                        None
                                    },
                                });
                                if request.kind == OperationKind::Move
                                    && outcome == ItemOutcome::Partial
                                {
                                    for pending_item in pending_items {
                                        receipts.push(ItemReceipt {
                                            item: pending_item,
                                            outcome: ItemOutcome::CancelledBeforeStart,
                                            detail: Some(
                                                "not started because an earlier Move Replace requires recovery".into(),
                                            ),
                                            created_folder_path: None,
                                            created_archive_path: None,
                                            move_receipt: None,
                                            replace_receipt: None, copy_undo: None,
                                        });
                                    }
                                    break;
                                }
                            }
                            Err(error) => {
                                let (outcome, detail) = copy_outcome(Err(error));
                                receipts.push(ItemReceipt {
                                    item,
                                    outcome,
                                    detail,
                                    created_folder_path: None,
                                    created_archive_path: None,
                                    move_receipt: None,
                                    replace_receipt: None,
                                    copy_undo: None,
                                });
                            }
                        }
                        continue;
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        receipts.push(ItemReceipt {
                            item,
                            outcome: ItemOutcome::Failed,
                            detail: Some("replace is not supported on this platform".into()),
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        });
                        continue;
                    }
                }
                Ok(WorkerControl::Cancel) | Err(_) => {
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::CancelledBeforeStart,
                        detail: Some("cancel requested".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                // An acknowledgement belongs to the later Replace barrier;
                // receiving one while this conflict is pending must not make
                // the item disappear from the factual terminal report.
                Ok(WorkerControl::ReplaceBackupJournaled { .. })
                | Ok(WorkerControl::ReplaceBackupJournaledExact { .. })
                | Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournaledExact { .. })
                | Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournalFailedExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenamePhaseJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenamePhaseJournalFailedExact { .. })
                | Ok(WorkerControl::RenameStageJournaledExact { .. })
                | Ok(WorkerControl::ZipCreateLedgerJournaledExact { .. })
                | Ok(WorkerControl::ZipCreateLedgerJournalFailedExact { .. })
                | Ok(WorkerControl::SevenZipCreateLedgerJournaledExact { .. })
                | Ok(WorkerControl::SevenZipCreateLedgerJournalFailedExact { .. })
                | Ok(WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { .. })
                | Ok(WorkerControl::ZipExtractLedgerJournaledExact { .. })
                | Ok(WorkerControl::ZipExtractLedgerJournalFailedExact { .. })
                | Ok(WorkerControl::BatchRenamePlanJournaledExact { .. })
                | Ok(WorkerControl::BatchRenameItemJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveManifestJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveChildIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveChildReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveRootIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveRootReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveCompleteJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplacePhaseJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplacePhaseJournalFailedExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceBackupIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceBackupReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceBackupCompleteJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenameBackupCompleteJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupManifestJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupChildIntentJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupChildReceiptJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupCompleteJournaledExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchJournaledExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchObservedExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchCleanupJournaledExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchJournalFailedExact { .. })
                | Ok(WorkerControl::SevenZipExtractTerminalJournaledExact { .. })
                | Ok(WorkerControl::SevenZipCreateCleanupJournaledExact { .. }) => {
                    // 7z extraction barriers are invalid for this generic path.
                    // These controls belong to a separate mutation barrier.
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Failed,
                        detail: Some("unexpected replace backup acknowledgement".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
            }
        }
        if move_guard.is_some() || recovery_copy {
            // Move と Recovery Copy の no-replace publish直前raceを検査するためのhook。productionはno-op。
            after_destination_probe();
        }
        let mut committed_destination = destination_path.clone();
        let mut copy_undo = None;
        let mut copy_completion_progress_counted = false;
        let mut result = match request.kind {
            OperationKind::Copy => {
                let copy_failure_evidence = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let copy_stage_custody = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let progress =
                    RefCell::new(|bytes, completed_files, discovered: Option<(u64, u64)>| {
                        counters.processed_bytes = counters.processed_bytes.saturating_add(bytes);
                        copy_completion_progress_counted |= completed_files > 0;
                        counters.processed_items =
                            counters.processed_items.saturating_add(completed_files);
                        if let Some((items, bytes)) = discovered {
                            counters.discovered_items =
                                counters.discovered_items.saturating_add(items);
                            counters.discovered_bytes =
                                counters.discovered_bytes.saturating_add(bytes);
                        }
                        sequence += 1;
                        emit(ExecutorFact::Progress {
                            token,
                            update: ProgressUpdate {
                                sequence,
                                counters,
                                current_item: Some(item),
                            },
                        });
                    });
                let destination = destination_path.as_ref().expect("copy has destination");
                let directory_copy_undo_candidates = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let directory_retry_provenance = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut result = {
                    #[cfg(windows)]
                    {
                        if source_is_directory && request.virtual_source.is_none() {
                            if copy_directory_retry_guard.is_some() {
                                windows::copy_directory_with_retry_guard(
                                    source, destination, token, item, cancelled,
                                    &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                    &mut || (progress.borrow_mut())(0, 1, None),
                                    &mut |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                                    &mut |receipt| directory_copy_undo_candidates.borrow_mut().push(receipt),
                                    copy_directory_retry_guard,
                                    &mut |provenance| directory_retry_provenance.borrow_mut().push(provenance),
                                )
                            } else {
                                let on_directory_undo = &mut |receipt| directory_copy_undo_candidates.borrow_mut().push(receipt);
                                if recovery_copy {
                                    windows::copy_entry_for_recovery_keep_both(
                                        source, destination, token, item, cancelled,
                                        &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                        &mut || (progress.borrow_mut())(0, 1, None),
                                        &mut |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                                        on_directory_undo,
                                    )
                                } else {
                                    windows::copy_entry_with_retry_provenance(
                                        source, destination, token, item, cancelled,
                                        &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                        &mut || (progress.borrow_mut())(0, 1, None),
                                        &mut |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                                        on_directory_undo,
                                        &mut |provenance| directory_retry_provenance.borrow_mut().push(provenance),
                                    )
                                }
                            }
                        } else if !source_is_directory
                            && copy_directory_retry_guard.is_none()
                            && request.virtual_source.is_none()
                            && !is_link_or_reparse_point(source)
                        {
                            match windows::copy_regular_file_with_undo_observed_and_custody(
                                Some(token),
                                source,
                                destination,
                                cancelled,
                                item,
                                &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                                &mut |provenance| {
                                    copy_failure_evidence.borrow_mut().push(provenance)
                                },
                                &mut |custody| copy_stage_custody.borrow_mut().push(custody),
                                copy_retry_guard,
                            ) {
                                    Ok(receipt) => {
                                        copy_undo = Some(receipt);
                                    (progress.borrow_mut())(0, 1, None);
                                    Ok(())
                                }
                                Err(error) => {
                                    let failure = Err(error);
                    if copy_retry_guard.is_none()
                        && copy_directory_retry_guard.is_none()
                        && operation_requires_elevation(&failure)
                                    {
                                        // 最初のadapterの証拠はgeneric fallbackの結果を表さない。
                                        // 別adapterへ入る前に、このitemのpending evidenceを破棄する。
                                        copy_failure_evidence
                                            .borrow_mut()
                                            .retain(|provenance| provenance.item != item);
                                        copy_entry_no_clobber(
                                            source,
                                            destination,
                                            token,
                                            item,
                                            cancelled,
                                            |bytes| (progress.borrow_mut())(bytes, 0, None),
                                            || (progress.borrow_mut())(0, 1, None),
                                            |items, bytes| {
                                                (progress.borrow_mut())(0, 0, Some((items, bytes)))
                                            },
                                        )
                                    } else {
                                        failure
                                    }
                                }
                            }
                        } else if copy_retry_guard.is_some() || copy_directory_retry_guard.is_some() {
                            Err(CopyError::GuardRejected(
                                "guarded Copy retry cannot use the generic Copy adapter".into(),
                            ))
                        } else {
                            copy_entry_no_clobber(
                                source,
                                destination,
                                token,
                                item,
                                cancelled,
                                |bytes| (progress.borrow_mut())(bytes, 0, None),
                                || (progress.borrow_mut())(0, 1, None),
                                |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                            )
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        if copy_retry_guard.is_some() || copy_directory_retry_guard.is_some() {
                            Err(CopyError::GuardRejected(
                                "guarded Copy retry requires its native typed adapter".into(),
                            ))
                        } else {
                            copy_entry_no_clobber(
                                source,
                                destination,
                                token,
                                item,
                                cancelled,
                                |bytes| (progress.borrow_mut())(bytes, 0, None),
                                || (progress.borrow_mut())(0, 1, None),
                                |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                            )
                        }
                    }
                };
                if recovery_copy && copy_directory_retry_guard.is_none()
                    && matches!(result, Err(CopyError::DestinationExists)) {
                    if source_is_directory {
                        let mut candidate = destination.to_path_buf();
                        result = copy_keep_both_directory_recovery(
                            source,
                            destination,
                            token,
                            item,
                            cancelled,
                            &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                            &mut || (progress.borrow_mut())(0, 1, None),
                            &mut |items, bytes| (progress.borrow_mut())(0, 0, Some((items, bytes))),
                            &mut |receipt| directory_copy_undo_candidates.borrow_mut().push(receipt),
                            &mut candidate,
                        );
                        if result.is_ok() {
                            committed_destination = Some(candidate);
                        }
                    } else {
                        result = match copy_keep_both_regular_file(
                            source,
                            destination,
                            token,
                            item,
                            cancelled,
                            &mut |bytes| (progress.borrow_mut())(bytes, 0, None),
                            &mut |provenance| copy_failure_evidence.borrow_mut().push(provenance),
                            &mut |custody| copy_stage_custody.borrow_mut().push(custody),
                        ) {
                            Ok(receipt) => {
                                committed_destination = Some(receipt.destination.clone());
                                copy_undo = Some(receipt);
                                (progress.borrow_mut())(0, 1, None);
                                Ok(())
                            }
                            Err(error) => Err(error),
                        };
                    }
                }
                drop(progress);
                for provenance in directory_retry_provenance.borrow_mut().drain(..) {
                    emit(ExecutorFact::CopyDirectoryRetryProvenance { token, provenance });
                }
                for provenance in copy_failure_evidence.borrow_mut().drain(..) {
                    emit(ExecutorFact::CopyFailureProvenance { token, provenance });
                }
                for custody in copy_stage_custody.borrow_mut().drain(..) {
                    emit(ExecutorFact::CopyStageCustody {
                        custody: super::runtime::CopyStageCustodyTransfer::new(custody),
                    });
                }
                if result.is_ok() {
                    for receipt in directory_copy_undo_candidates.borrow_mut().drain(..) {
                        emit(ExecutorFact::DirectoryCopyUndoCandidate { token, item, receipt });
                    }
                }
                result
            }
            OperationKind::Move => {
                let destination_path = destination_path.as_ref().expect("move has destination");
                #[cfg(windows)]
                if directory_move_cross_volume {
                    let (source_identity, destination_identity) = directory_move_identities
                        .as_ref()
                        .expect("cross-volume directory routing has pinned identities");
                    let event_sink = std::cell::RefCell::new(&mut emit);
                    let mut on_bytes = |bytes| {
                        counters.processed_bytes = counters.processed_bytes.saturating_add(bytes);
                        sequence += 1;
                        (event_sink.borrow_mut())(ExecutorFact::Progress {
                            token,
                            update: ProgressUpdate {
                                sequence,
                                counters,
                                current_item: Some(item),
                            },
                        });
                    };
                    let mut emit_fact = |fact| (event_sink.borrow_mut())(fact);
                    let result = windows::move_cross_volume_directory(
                        token,
                        item,
                        source,
                        destination_path,
                        source_identity,
                        destination_identity,
                        cancelled,
                        controls,
                        &mut on_bytes,
                        &mut emit_fact,
                    );
                    // Runtime は各 item の候補を端末 receipt と照合してから一括で採用する。
                    if !directory_move_replace_enabled
                        && let Ok(receipt) = &result
                    {
                        (event_sink.borrow_mut())(ExecutorFact::MoveUndoCandidate {
                            token,
                            item,
                            receipt: receipt.clone(),
                        });
                    }
                    result.map(|_| ())
                } else if let Some((source_identity, destination_identity)) = move_guard {
                    let result = move_file_guarded_with_undo(
                        source,
                        destination_path,
                        source_identity,
                        destination_identity,
                        cancelled,
                        &mut |receipt| emit(ExecutorFact::MoveUndoCandidate { token, item, receipt }),
                    );
                    if should_emit_guarded_move_no_effect_proof(&result) {
                        emit(ExecutorFact::GuardedMoveNoEffectProof {
                            token,
                            proof: super::model::GuardedMoveNoEffectProof {
                                item,
                                source: source.to_path_buf(),
                                destination: destination_path.to_path_buf(),
                                source_identity: source_identity.clone(),
                                destination_parent_identity: destination_identity.clone(),
                            },
                        });
                    }
                    result
                } else {
                    let event_sink = std::cell::RefCell::new(&mut emit);
                    let on_bytes = |bytes| {
                        counters.processed_bytes = counters.processed_bytes.saturating_add(bytes);
                        sequence += 1;
                        (event_sink.borrow_mut())(ExecutorFact::Progress {
                            token,
                            update: ProgressUpdate {
                                sequence,
                                counters,
                                current_item: Some(item),
                            },
                        });
                    };
                    let mut retained = |proof| (event_sink.borrow_mut())(ExecutorFact::PublishedMoveSourceRetained { token, proof });
                    let mut disposition = |proof: super::model::PublishedMoveDispositionProof| {
                        let undo_receipt = proof.content_sha256.as_ref().filter(|hash|
                            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && proof.source_pre_disposition_signature.is_reliable_for_cross_volume_move_retry()
                                && proof.destination_signature.is_reliable_for_cross_volume_move_retry()
                        ).map(|content_sha256| super::model::MoveUndoReceipt {
                            source: proof.source.clone(),
                            destination: proof.destination.clone(),
                            object_identity: proof.destination_identity.clone(),
                            object_kind: super::model::MoveUndoObjectKind::RegularFile,
                            source_parent_route: proof.source_parent_route.clone(),
                            destination_parent_route: proof.destination_parent_route.clone(),
                            cross_volume_file: Some(super::model::CrossVolumeMoveUndoProof {
                                original_source_identity: proof.source_identity.clone(),
                                original_source_signature: proof.source_pre_disposition_signature,
                                destination_signature: proof.destination_signature,
                                content_sha256: content_sha256.clone(),
                                directory_manifest: None,
                            }),
                        });
                        (event_sink.borrow_mut())(ExecutorFact::PublishedMoveDisposition { token, proof });
                        if let Some(receipt) = undo_receipt {
                            (event_sink.borrow_mut())(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                        }
                    };
                    let mut no_effect = |proof| (event_sink.borrow_mut())(ExecutorFact::CrossVolumeMoveNoEffectProof { token, proof });
                    if let Some(destination_identity) = request.move_destination_identity_guard.as_ref() {
                        move_file_clipboard_worker_observed_with_undo(
                            source, destination_path, destination_identity,
                            clipboard_move_source,
                            cancelled, on_bytes, item,
                            cross_volume_move_retry_guard, &mut retained, &mut disposition, &mut no_effect,
                            &mut |receipt| (event_sink.borrow_mut())(ExecutorFact::MoveUndoCandidate { token, item, receipt }),
                        )
                    } else {
                        move_file_observed(source, destination_path, cancelled, on_bytes, item, cross_volume_move_retry_guard, &mut retained, &mut disposition, &mut no_effect)
                    }
                }
            }
            OperationKind::CreateSymbolicLink => create_symbolic_link(
                source,
                destination_path
                    .as_ref()
                    .expect("symbolic-link creation has destination"),
                request.create_parent_directory_identity_guard.as_ref(),
                cancelled,
            ),
            OperationKind::CreateJunction => create_junction(
                source,
                destination_path
                    .as_ref()
                    .expect("junction creation has destination"),
                cancelled,
            ),
            OperationKind::Trash => match request
                .trash_source_guards
                .as_ref()
                .and_then(|guards| guards.get(item.0 as usize))
                .ok_or_else(|| CopyError::Unknown("Trash source identity is missing".into()))
                .and_then(|guard| trash_adapter(source, guard, trash_tag, cancelled))
            {
                Ok(receipt) => {
                    emit(ExecutorFact::TrashReceipt { token, item, receipt });
                    Ok(())
                }
                Err(error) => Err(error),
            },
            OperationKind::ExternalMoveCleanup => unreachable!("handled before item execution"),
            OperationKind::CopyStageCleanup => unreachable!("handled before item execution"),
            OperationKind::CreateFolder => unreachable!("handled before item execution"),
            OperationKind::CreateFile => unreachable!("handled before item execution"),
            OperationKind::CreateShortcut => unreachable!("handled before item execution"),
            OperationKind::CreateZip => unreachable!("handled before item execution"),
            OperationKind::Create7z => unreachable!("handled before item execution"),
            OperationKind::ExtractZip => unreachable!("handled before item execution"),
            OperationKind::Extract7z => unreachable!("handled before item execution"),
            OperationKind::Rename => rename_file_in_parent(
                source,
                destination_path.as_ref().expect("rename has target"),
                request
                    .move_identity_guard
                    .as_ref()
                    .map(|guard| (&guard.source_identities[0], &guard.destination_identity)),
                request.rename_reparse_guard.as_ref(),
                None,
                token,
                item,
                cancelled,
                controls,
                &mut emit,
                true,
            ),
            OperationKind::BatchRename | OperationKind::BatchRenameUndo => {
                let target = request
                    .batch_rename_plan
                    .as_ref()
                    .and_then(|plan| plan.get(item.0 as usize));
                match target {
                    Some(target)
                        if target.reparse_guard.as_ref().is_none_or(|guard| {
                            guard.source_identity == target.source_identity
                                && guard.parent_identity == target.parent_identity
                        }) => rename_file_in_parent(
                        source,
                        destination_path.as_ref().expect("batch rename target"),
                        Some((&target.source_identity, &target.parent_identity)),
                        target.reparse_guard.as_ref(),
                        Some(target.kind),
                        token,
                        item,
                        cancelled,
                        controls,
                        &mut emit,
                        false,
                    ),
                    Some(_) | None => Err(CopyError::Unsupported),
                }
            }
                };
                if request.kind == OperationKind::Move
            && move_guard.is_some()
            && matches!(&result, Err(CopyError::DestinationExists))
        {
            let destination_path = destination_path
                .as_ref()
                .expect("guarded move has destination");
            let notice = super::model::ConflictNotice {
                item,
                probe_revision: sequence.wrapping_add(1),
                description: format!("{} already exists", destination_path.display()),
                replace_supported: false,
                directory_move_replace_rename_candidate: None,
            };
            let directory_move_replace_candidate =
                notice.directory_move_replace_rename_candidate.clone();
            let automatic = conflict_policy.filter(|decision| {
                *decision != super::model::ConflictDecision::Replace && !notice.replace_supported
            });
            if automatic.is_none() {
                emit(ExecutorFact::Conflict { token, notice });
            }
            let response = automatic.map_or_else(
                || controls.recv(),
                |decision| {
                    Ok(WorkerControl::Resolve {
                        decision,
                        apply_to_remaining: false,
                    })
                },
            );
            match response {
                Ok(WorkerControl::Resolve {
                    decision: super::model::ConflictDecision::Skip,
                    apply_to_remaining,
                }) => {
                    if apply_to_remaining {
                        conflict_policy = Some(super::model::ConflictDecision::Skip);
                    }
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Skipped,
                        detail: None,
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                Ok(WorkerControl::Resolve {
                    decision: super::model::ConflictDecision::KeepBoth,
                    apply_to_remaining,
                }) => {
                    if apply_to_remaining {
                        conflict_policy = Some(super::model::ConflictDecision::KeepBoth);
                    }
                    let (source_identity, destination_identity) =
                        move_guard.expect("late guarded-move collision has identity guard");
                    let mut resolved = destination_path.clone();
                    result = move_keep_both_guarded(
                        source,
                        destination_path,
                        source_identity,
                        destination_identity,
                        cancelled,
                        |candidate| resolved = candidate.to_path_buf(),
                        &mut |_| {},
                    );
                    committed_destination = Some(resolved);
                }
                Ok(WorkerControl::DirectoryMoveReplaceRenameApproved {
                    token: approved_token,
                    candidate,
                }) => {
                    let exact_approval = approved_token.attempt_token() == token
                        && approved_token.item_id() == item
                        && directory_move_replace_candidate.as_ref() == Some(&candidate);
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Failed,
                        detail: Some(if exact_approval {
                            "same-volume directory Replace remains gated".into()
                        } else {
                            "mismatched same-volume directory Replace approval".into()
                        }),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                Ok(WorkerControl::Resolve {
                    decision: super::model::ConflictDecision::Replace,
                    ..
                }) => {
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Failed,
                        detail: Some("replace is not supported for identity-guarded moves".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                Ok(WorkerControl::Cancel) | Err(_) => {
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::CancelledBeforeStart,
                        detail: Some("cancel requested".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
                Ok(WorkerControl::ReplaceBackupJournaled { .. })
                | Ok(WorkerControl::ReplaceBackupJournaledExact { .. })
                | Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournaledExact { .. })
                | Ok(WorkerControl::CrossVolumeMoveReplacePhaseJournalFailedExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenamePhaseJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenamePhaseJournalFailedExact { .. })
                | Ok(WorkerControl::RenameStageJournaledExact { .. })
                | Ok(WorkerControl::ZipCreateLedgerJournaledExact { .. })
                | Ok(WorkerControl::ZipCreateLedgerJournalFailedExact { .. })
                | Ok(WorkerControl::SevenZipCreateLedgerJournaledExact { .. })
                | Ok(WorkerControl::SevenZipCreateLedgerJournalFailedExact { .. })
                | Ok(WorkerControl::SevenZipCreateLedgerJournaledCancelledExact { .. })
                | Ok(WorkerControl::ZipExtractLedgerJournaledExact { .. })
                | Ok(WorkerControl::ZipExtractLedgerJournalFailedExact { .. })
                | Ok(WorkerControl::BatchRenamePlanJournaledExact { .. })
                | Ok(WorkerControl::BatchRenameItemJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveManifestJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveChildIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveChildReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveRootIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveRootReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveCompleteJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplacePhaseJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplacePhaseJournalFailedExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceBackupIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceBackupReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceBackupCompleteJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupIntentJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupReceiptJournaledExact { .. })
                | Ok(WorkerControl::DirectoryMoveReplaceRenameBackupCompleteJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupManifestJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupChildIntentJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupChildReceiptJournaledExact { .. })
                | Ok(WorkerControl::CopyReplaceCleanupCompleteJournaledExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchJournaledExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchObservedExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchCleanupJournaledExact { .. })
                | Ok(WorkerControl::SevenZipExtractBatchJournalFailedExact { .. })
                | Ok(WorkerControl::SevenZipExtractTerminalJournaledExact { .. })
                | Ok(WorkerControl::SevenZipCreateCleanupJournaledExact { .. }) => {
                    receipts.push(ItemReceipt {
                        item,
                        outcome: ItemOutcome::Failed,
                        detail: Some("invalid response to guarded move conflict".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    });
                    continue;
                }
            }
        }
        let destination_published_override =
            matches!(&result, Err(CopyError::PrePublishPartial(_))).then_some(false);
        let (mut outcome, mut detail) = copy_outcome(result);
        if matches!(
            request.kind,
            OperationKind::BatchRename | OperationKind::BatchRenameUndo
        ) && outcome == ItemOutcome::Completed
        {
            #[cfg(all(test, target_os = "windows"))]
            tests::checkpoint_batch_rename_after_mutation();
            let target = request
                .batch_rename_plan
                .as_ref()
                .and_then(|plan| plan.get(item.0 as usize));
            if let Some(target) = target {
                emit(ExecutorFact::BatchRenameItemCompleted {
                    token,
                    item,
                    source: target.source.clone(),
                    destination: target.destination.clone(),
                    source_identity: format!("{:?}", target.source_identity),
                    parent_identity: format!("{:?}", target.parent_identity),
                });
                match controls.recv() {
                    Ok(WorkerControl::BatchRenameItemJournaledExact {
                        token: ack_token,
                        item: ack_item,
                        source,
                        destination,
                    }) if ack_token == token
                        && ack_item == item
                        && source == target.source
                        && destination == target.destination => {}
                    _ => {
                        outcome = ItemOutcome::Unknown;
                        detail = Some(
                            "rename completed but its receipt was not durably acknowledged".into(),
                        );
                    }
                }
            } else {
                outcome = ItemOutcome::Unknown;
                detail = Some("batch rename plan changed during execution".into());
            }
        }
        // Copy completion callback が数えた場合は、共通処理で重ねて加算しない。
        if outcome == ItemOutcome::Completed
            && (!source_is_directory || request.kind == OperationKind::Move)
            && !(request.kind == OperationKind::Copy && copy_completion_progress_counted)
        {
            counters.processed_items += 1;
        }
        receipts.push(item_receipt(
            request.kind,
            item,
            source,
            committed_destination.as_deref(),
            outcome,
            detail,
            destination_published_override,
            copy_undo.filter(|_| outcome == ItemOutcome::Completed),
        ));
        #[cfg(windows)]
        if directory_move_cross_volume && outcome != ItemOutcome::Completed {
            for suffix in pending_items {
                receipts.push(ItemReceipt {
                    item: suffix,
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: Some(
                        "not started because an earlier directory Move requires recovery".into(),
                    ),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                });
            }
            break;
        }
        if matches!(
            request.kind,
            OperationKind::BatchRename | OperationKind::BatchRenameUndo
        ) && outcome != ItemOutcome::Completed
        {
            for suffix in pending_items {
                receipts.push(ItemReceipt {
                    item: suffix,
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: Some("cancelled because an earlier batch rename item failed".into()),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                });
            }
            break;
        }
    }
    let completed = receipts
        .iter()
        .filter(|receipt| receipt.outcome == ItemOutcome::Completed)
        .count();
    let changed = receipts
        .iter()
        .filter(|receipt| {
            matches!(
                receipt.outcome,
                ItemOutcome::Completed | ItemOutcome::Partial
            )
        })
        .count();
    let uncertain_rename = receipts.iter().any(|receipt| {
        receipt.outcome == ItemOutcome::Unknown
            && matches!(
                request.kind,
                OperationKind::Rename | OperationKind::BatchRename | OperationKind::BatchRenameUndo
            )
    });
    let uncertain_move = request.kind == OperationKind::Move
        && receipts.iter().any(|receipt| receipt.outcome == ItemOutcome::Unknown);
    // Shell が mutation の可能性を報告した Trash は Unknown のまま親を再列挙する。
    let uncertain_trash = has_uncertain_trash_result(request.kind, &receipts);
    let outcome = if completed == receipts.len() {
        TerminalOutcome::Succeeded
    } else if changed > 0 {
        TerminalOutcome::Partial
    } else if receipts
        .iter()
        .any(|receipt| receipt.outcome == ItemOutcome::Unknown)
    {
        TerminalOutcome::Unknown
    } else if cancelled.load(Ordering::Acquire) {
        TerminalOutcome::Cancelled
    } else {
        TerminalOutcome::Failed
    };
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: counters,
            receipts,
            retry: if outcome == TerminalOutcome::Succeeded || exact_directory_replace_no_effect {
                RetryPlan::NotAllowed
            } else {
                RetryPlan::RequiresReinspection
            },
            change_hints: (changed > 0 || uncertain_rename || uncertain_move || uncertain_trash)
                .then(|| change_hints(request.kind, &request.sources, destination))
                .unwrap_or_default(),
        },
    });
}

fn preflight_local_move_guard(
    sources: &[PathBuf],
    destination_identity: &crate::filesystem_identity::FilesystemIdentity,
) -> Option<super::model::MoveIdentityGuard> {
    if sources.is_empty() {
        return None;
    }
    let mut source_identities = Vec::with_capacity(sources.len());
    for source in sources {
        let metadata = std::fs::symlink_metadata(source).ok()?;
        let kind = if metadata.file_type().is_symlink() {
            return None;
        } else if metadata.is_file() {
            crate::directory::EntryKind::File
        } else if metadata.is_dir() {
            crate::directory::EntryKind::Folder
        } else {
            return None;
        };
        source_identities.push(
            crate::directory::EntrySourceIdentity::from_path(source, kind)?.filesystem_identity(),
        );
    }
    Some(super::model::MoveIdentityGuard {
        source_identities,
        destination_identity: destination_identity.clone(),
            })
}

/// Clipboard Paste captures only the narrow Replace proof it can establish
/// from one no-follow regular-file handle. The executor still reopens and
/// compares this identity before offering or performing Replace.
pub(crate) fn capture_move_replace_source_evidence(
    source: &Path,
    destination_identity: &crate::filesystem_identity::FilesystemIdentity,
) -> Option<super::model::MoveReplaceSourceEvidence> {
    #[cfg(windows)]
    {
        let opened = windows::prepare_regular_file_source(source).ok()?;
        let source_identity = opened.shared_identity()?;
        let (source_volume, _) = source_identity.windows_parts()?;
        let (destination_volume, _) = destination_identity.windows_parts()?;
        (source_volume == destination_volume).then_some(
            super::model::MoveReplaceSourceEvidence { source_identity },
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (source, destination_identity);
        None
    }
}

fn execute_copy_stage_cleanup(
    token: AttemptToken,
    request: &super::model::OperationRequest,
    lease: Option<super::runtime::CopyStageCleanupLease>,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let Some(lease) = lease else {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    };
    if request.sources.as_slice() != [lease.path.as_path()]
        || request.destination.is_some()
        || request.virtual_source.is_some()
        || !request.trash_source_guards.is_none()
        || request.kind != OperationKind::CopyStageCleanup
    {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    let item = lease.item;
    let stage_path = lease.path.clone();
    let (outcome, detail) = if cancelled.load(Ordering::Acquire) {
        (
            super::model::ItemOutcome::CancelledBeforeStart,
            Some("cleanup cancelled before disposition".into()),
        )
    } else {
        #[cfg(windows)]
        let disposition = windows::cleanup_copy_stage_handle(lease, cancelled)
            .map_err(|error| format!("{error:?}"));
        #[cfg(not(windows))]
        let disposition: Result<bool, String> =
            Err("held-handle Copy stage cleanup is Windows-only".into());
        match disposition {
            Ok(true) => (super::model::ItemOutcome::Completed, None),
            Ok(false) => (
                super::model::ItemOutcome::Unknown,
                Some("stage disposition or close could not be verified".into()),
            ),
            Err(error) => (super::model::ItemOutcome::Failed, Some(error)),
        }
    };
    let terminal = match outcome {
        super::model::ItemOutcome::Completed => TerminalOutcome::Succeeded,
        super::model::ItemOutcome::CancelledBeforeStart => TerminalOutcome::Cancelled,
        super::model::ItemOutcome::Unknown => TerminalOutcome::Unknown,
        _ => TerminalOutcome::Failed,
    };
    let mut report = rejected();
    report.outcome = terminal;
    report.final_counters = ProgressCounters {
        processed_items: u64::from(outcome == super::model::ItemOutcome::Completed),
        discovered_items: 1,
        total_items: Some(1),
        ..ProgressCounters::default()
    };
    report.receipts.push(super::model::ItemReceipt {
        item,
        outcome,
        detail,
        created_folder_path: None,
        created_archive_path: None,
        move_receipt: None,
        replace_receipt: None,
        copy_undo: None,
    });
    report.change_hints = vec![super::model::FsChangeHint {
        directory: stage_path.parent().unwrap_or(Path::new("")).to_path_buf(),
    }];
    emit(ExecutorFact::Terminal { token, report });
}

/// A cleanup worker whose Running record was not durably acknowledged must
/// close without ever reaching the held-handle disposition call.
pub(crate) fn copy_stage_cleanup_running_ack_failed(
    token: AttemptToken,
    lease: Option<super::runtime::CopyStageCleanupLease>,
    cancelled: bool,
) -> ExecutorFact {
    let (item, path) = lease
        .as_ref()
        .map(|lease| (lease.item, Some(lease.path.clone())))
        .unwrap_or((super::model::ItemId(0), None));
    let outcome = if cancelled {
        super::model::ItemOutcome::CancelledBeforeStart
    } else {
        super::model::ItemOutcome::Failed
    };
    let mut report = rejected();
    report.outcome = if cancelled {
        TerminalOutcome::Cancelled
    } else {
        TerminalOutcome::Failed
    };
    report.final_counters = ProgressCounters {
        discovered_items: 1,
        total_items: Some(1),
        ..ProgressCounters::default()
    };
    report.receipts.push(super::model::ItemReceipt {
        item,
        outcome,
        detail: Some(if cancelled {
            "cleanup cancelled before Running journal acknowledgment".into()
        } else {
            "Running journal acknowledgment was not accepted".into()
        }),
        created_folder_path: None,
        created_archive_path: None,
        move_receipt: None,
        replace_receipt: None,
        copy_undo: None,
    });
    if let Some(path) = path {
        report.change_hints = vec![super::model::FsChangeHint {
            directory: path.parent().unwrap_or(Path::new("")).to_path_buf(),
        }];
    }
    // Dropping the lease consumes process-local custody; it is never rebuilt
    // from the journal path when the ACK is missing or rejected.
    drop(lease);
    ExecutorFact::Terminal { token, report }
}

fn execute_external_move_cleanup(
    token: AttemptToken,
    request: &super::model::OperationRequest,
    retry_items: &[ItemId],
    lease: Option<super::runtime::SourceCleanupLease>,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let guard = request.source_cleanup_identity_guard.as_ref();
    let valid = request.destination.is_none()
        && request.move_identity_guard.is_none()
        && !request.sources.is_empty()
        && u32::try_from(request.sources.len() - 1).is_ok()
        && retry_items.is_empty()
        && lease.as_ref().is_some_and(|lease| {
            lease.paths == request.sources
                && lease.handles.len() == request.sources.len()
                && lease.parent_routes.len() == request.sources.len()
                && lease.leaf_signatures.len() == request.sources.len()
        })
        && guard.is_some_and(|guard| {
            guard.source_identities.len() == request.sources.len()
                && guard
                    .source_identities
                    .iter()
                    .all(|identity| identity.windows_parts().is_some())
                && guard.parent_routes.len() == request.sources.len()
                && guard.parent_routes.iter().all(|route| {
                    !route.is_empty()
                        && route
                            .iter()
                            .all(|parent| parent.identity.windows_parts().is_some())
                })
                && guard.leaf_signatures.len() == request.sources.len()
                && guard
                    .leaf_signatures
                    .iter()
                    .all(|signature| signature.number_of_links > 0)
                && lease.as_ref().is_some_and(|lease| {
                    guard.parent_routes == lease.parent_routes
                        && guard.leaf_signatures == lease.leaf_signatures
                })
                && request
                    .sources
                    .iter()
                    .zip(&guard.parent_routes)
                    .all(|(source, route)| {
                        let Some(parent) = source.parent() else {
                            return false;
                        };
                        let mut expected = parent
                            .ancestors()
                            .map(Path::to_path_buf)
                            .collect::<Vec<_>>();
                        expected.reverse();
                        route.len() == expected.len()
                            && route
                                .iter()
                                .zip(expected)
                                .all(|(captured, expected)| captured.path == expected)
                    })
        });
    if !valid {
        emit(ExecutorFact::Terminal {
            token,
            report: rejected(),
        });
        return;
    }
    let guard = guard.unwrap();
    let mut leased_handles = lease.unwrap().handles.into_iter();
    let mut receipts = Vec::with_capacity(request.sources.len());
    let mut deleted_sources = Vec::new();
    let mut counters = ProgressCounters {
        total_items: Some(request.sources.len() as u64),
        ..ProgressCounters::default()
    };
    let mut sequence = 0;
    let mut cancelled_before_next = false;
    for (index, (source, identity)) in request
        .sources
        .iter()
        .zip(&guard.source_identities)
        .enumerate()
    {
        let item = ItemId(u32::try_from(index).expect("validated cleanup item count"));
        let (outcome, detail) = if cancelled_before_next || cancelled.load(Ordering::Acquire) {
            cancelled_before_next = true;
            (
                ItemOutcome::CancelledBeforeStart,
                Some("cancel requested before disposition".into()),
            )
        } else {
            #[cfg(windows)]
            let outcome = windows::cleanup_external_move_source(
                source,
                identity,
                &guard.parent_routes[index],
                &guard.leaf_signatures[index],
                leased_handles
                    .next()
                    .expect("validated cleanup lease length"),
                cancelled,
            );
            #[cfg(not(windows))]
            let outcome: Result<(), &'static str> =
                Err("source cleanup is unsupported on this platform");
            match outcome {
                Ok(()) => {
                    deleted_sources.push(source.clone());
                    (ItemOutcome::Completed, None)
                }
                Err("cancelled") => (
                    ItemOutcome::CancelledBeforeStart,
                    Some("cancel requested before disposition".into()),
                ),
                Err(reason) => (ItemOutcome::Partial, Some(reason.into())),
            }
        };
        receipts.push(ItemReceipt {
            item,
            outcome,
            detail,
            created_folder_path: None,
            created_archive_path: None,
            move_receipt: None,
            replace_receipt: None,
            copy_undo: None,
        });
        if outcome == ItemOutcome::Completed {
            counters.processed_items += 1;
        }
        sequence += 1;
        emit(ExecutorFact::Progress {
            token,
            update: ProgressUpdate {
                sequence,
                counters,
                current_item: Some(item),
            },
        });
    }
    let complete = receipts
        .iter()
        .all(|receipt| receipt.outcome == ItemOutcome::Completed);
    let changed = counters.processed_items > 0;
    let partial = receipts
        .iter()
        .any(|receipt| receipt.outcome == ItemOutcome::Partial);
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: if complete {
                TerminalOutcome::Succeeded
            } else if partial || changed {
                TerminalOutcome::Partial
            } else {
                TerminalOutcome::Cancelled
            },
            final_counters: counters,
            receipts,
            retry: if complete {
                RetryPlan::NotAllowed
            } else {
                RetryPlan::RequiresReinspection
            },
            change_hints: change_hints(OperationKind::ExternalMoveCleanup, &deleted_sources, None),
        },
    });
}

/// A move first uses the same-volume handle rename. Only a separately proven
/// cross-volume regular file is allowed to use the copy/publish/recheck/delete
/// adapter; a generic rename failure never enables that fallback.
fn move_file(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> Result<(), CopyError> {
    move_file_observed(source, destination, cancelled, &mut on_bytes, super::model::ItemId(0), None, &mut |_| {}, &mut |_| {}, &mut |_| {})
}

fn move_file_clipboard_worker_observed_with_undo(
    source: &Path,
    destination: &Path,
    destination_identity: &crate::filesystem_identity::FilesystemIdentity,
    expected_source: Option<&super::model::ClipboardMoveSourceEvidence>,
    cancelled: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
    item: super::model::ItemId,
    retry_guard: Option<&super::model::CrossVolumeMoveRetryGuard>,
    on_source_retained: &mut dyn FnMut(super::model::PublishedMoveSourceRetainedProof),
    on_disposition: &mut dyn FnMut(super::model::PublishedMoveDispositionProof),
    on_no_effect: &mut dyn FnMut(super::model::CrossVolumeMoveNoEffectProof),
    on_undo: &mut dyn FnMut(super::model::MoveUndoReceipt),
) -> Result<(), CopyError> {
    if let Some(expected) = expected_source {
        return match super::rename::move_same_volume_no_replace_clipboard_with_undo(
            source, destination, &expected.source_identity, destination_identity, cancelled,
        ) {
            Ok(Some(receipt)) => { on_undo(receipt); Ok(()) }
            Ok(None) => Err(CopyError::Unsupported),
            Err(super::rename::RenameError::DestinationExists) => Err(CopyError::DestinationExists),
            Err(super::rename::RenameError::Cancelled) => Err(CopyError::Cancelled),
            Err(super::rename::RenameError::MutationUncertain(detail)) => Err(CopyError::Unknown(detail)),
            Err(super::rename::RenameError::Partial(detail)) => Err(CopyError::Partial(detail)),
            Err(super::rename::RenameError::CrossVolume) => {
                if expected.reparse_tag.is_some() {
                    return match super::privileged::move_link_object_direct_with_evidence(
                        source, destination, Some(expected), Some(destination_identity),
                    ) {
                        Ok(super::privileged::LinkMoveResult::Completed) => Ok(()),
                        Ok(super::privileged::LinkMoveResult::Partial(detail)) => Err(CopyError::Partial(detail)),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(CopyError::DestinationExists),
                        Err(error) if super::privileged::requires_elevation(&error) => {
                            match super::privileged::session_broker().move_link_object_with_evidence(
                                source, destination, expected, destination_identity,
                                || cancelled.load(Ordering::Acquire),
                            ) {
                                Ok(super::privileged::LinkMoveResult::Completed) => Ok(()),
                                Ok(super::privileged::LinkMoveResult::Partial(detail)) => Err(CopyError::Partial(detail)),
                                Err(detail) => Err(CopyError::Io(detail)),
                            }
                        }
                        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
                    };
                }
                #[cfg(target_os = "windows")]
                {
                    windows::move_cross_volume_regular_file(
                        source, destination, cancelled, &mut on_bytes, &mut || {}, item,
                        retry_guard, Some(&expected.source_identity), Some(destination_identity), on_source_retained, on_disposition,
                        on_no_effect,
                    )
                }
                #[cfg(not(target_os = "windows"))]
                { Err(CopyError::Unsupported) }
            }
            Err(error @ (super::rename::RenameError::IdentityMismatch
                | super::rename::RenameError::ReparsePoint
                | super::rename::RenameError::Unsupported)) => Err(CopyError::GuardRejected(format!("{error:?}"))),
            Err(error) => Err(CopyError::Io(format!("{error:?}"))),
        };
    }
    match super::rename::move_same_volume_no_replace_worker_observed_with_undo(
        source, destination, destination_identity, cancelled,
    ) {
        Ok(Some(receipt)) => {
            on_undo(receipt);
            Ok(())
        }
        Ok(None) => move_file_observed(source, destination, cancelled, on_bytes, item, retry_guard, on_source_retained, on_disposition, on_no_effect),
        Err(super::rename::RenameError::CrossVolume | super::rename::RenameError::Unsupported) => {
            move_file_observed(source, destination, cancelled, on_bytes, item, retry_guard, on_source_retained, on_disposition, on_no_effect)
        }
        Err(super::rename::RenameError::DestinationExists) => Err(CopyError::DestinationExists),
        Err(super::rename::RenameError::Cancelled) => Err(CopyError::Cancelled),
        Err(super::rename::RenameError::MutationUncertain(detail)) => Err(CopyError::Unknown(detail)),
        Err(super::rename::RenameError::Partial(detail)) => Err(CopyError::Partial(detail)),
        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
    }
}

fn move_file_observed(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
    item: super::model::ItemId,
    retry_guard: Option<&super::model::CrossVolumeMoveRetryGuard>,
    mut on_source_retained: impl FnMut(super::model::PublishedMoveSourceRetainedProof),
    mut on_disposition: impl FnMut(super::model::PublishedMoveDispositionProof),
    mut on_no_effect: impl FnMut(super::model::CrossVolumeMoveNoEffectProof),
) -> Result<(), CopyError> {
    if retry_guard.is_none() && is_link_or_reparse_point(source) {
        return move_link_object(source, destination, cancelled);
    }
    let mut no_effect = None;
    let mut after_publish = || {
        #[cfg(test)]
        tests::pause_cross_volume_move_after_publish_for_test(source, destination, item);
    };
    let result = move_file_without_broker_observed(
        source,
        destination,
        cancelled,
        &mut on_bytes,
        &mut after_publish,
        item,
        retry_guard,
        &mut on_source_retained,
        &mut on_disposition,
        &mut |proof| no_effect = Some(proof),
    );
    if !operation_requires_elevation(&result) {
        if let Some(proof) = no_effect { on_no_effect(proof); }
        return result;
    }
    if retry_guard.is_some() { return Err(CopyError::Unsupported); }
    super::privileged::session_broker()
        .move_regular_file(source, destination, || cancelled.load(Ordering::Acquire))
        .map_err(CopyError::Io)
}

fn cross_volume_retry_guard_has_reliable_signature(
    guard: &super::model::CrossVolumeMoveRetryGuard,
) -> bool {
    guard
        .proof
        .source_signature
        .is_reliable_for_cross_volume_move_retry()
}

#[cfg(test)]
mod cross_volume_retry_guard_tests {
    use super::cross_volume_retry_guard_has_reliable_signature;
    use crate::operations::model::{
        CrossVolumeMoveNoEffectProof, CrossVolumeMoveRetryGuard, ItemId,
        SourceCleanupLeafSignature,
    };
    use std::path::PathBuf;

    #[test]
    fn executor_guard_rejects_fabricated_zero_change_time_before_staging() {
        let make_guard = |change_time| CrossVolumeMoveRetryGuard {
            proof: CrossVolumeMoveNoEffectProof {
                item: ItemId(0),
                source: PathBuf::from("source"),
                destination: PathBuf::from("target/source"),
                source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
                source_signature: SourceCleanupLeafSignature { number_of_links: 1, change_time },
                destination_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(3, 4),
            },
        };
        assert!(!cross_volume_retry_guard_has_reliable_signature(&make_guard(0)));
        assert!(cross_volume_retry_guard_has_reliable_signature(&make_guard(1)));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires distinct fixed NTFS C: and E: fixtures"]
    fn cross_volume_adapter_rejects_zero_change_time_guard_before_stage() {
        use std::sync::atomic::AtomicBool;

        let destination_root = std::env::var_os("GPUI_FILER_CROSS_VOLUME_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .expect(r"GPUI_FILER_CROSS_VOLUME_TEST_ROOT must name E:\Temp");
        let source_temp = tempfile::Builder::new()
            .prefix("gpui-filer-zero-change-time-source-")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        let destination_temp = tempfile::Builder::new()
            .prefix("gpui-filer-zero-change-time-destination-")
            .tempdir_in(destination_root)
            .unwrap();
        let source = source_temp.path().join("payload.bin");
        let destination_parent = destination_temp.path().join("destination");
        std::fs::create_dir(&destination_parent).unwrap();
        let destination = destination_parent.join("payload.bin");
        let bytes = b"unchanged zero-token source";
        std::fs::write(&source, bytes).unwrap();
        let source_file = std::fs::File::open(&source).unwrap();
        let source_signature = super::windows::leaf_signature(&source_file).unwrap();
        assert_eq!(source_signature.number_of_links, 1);
        assert_eq!(source_signature.change_time, 0, "fixture must expose the observed zero token");
        let source_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &source,
            crate::directory::EntryKind::File,
        )
        .unwrap()
        .filesystem_identity();
        let destination_parent_identity =
            super::windows::directory_identity_for_test(&destination_parent).unwrap();
        let guard = CrossVolumeMoveRetryGuard {
            proof: CrossVolumeMoveNoEffectProof {
                item: ItemId(0),
                source: source.clone(),
                destination: destination.clone(),
                source_identity,
                source_signature,
                destination_parent_identity,
            },
        };

        let result = super::move_file_observed(
            &source,
            &destination,
            &AtomicBool::new(false),
            |_| {},
            ItemId(0),
            Some(&guard),
            |_| {},
            |_| {},
            |_| {},
        );
        assert!(matches!(result, Err(super::CopyError::Unsupported)));
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(&destination_parent).unwrap().count(), 0);
    }
}

fn move_file_guarded(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    match super::rename::move_same_volume_no_replace_guarded(
        source,
        destination,
        expected_source,
        expected_destination_directory,
        cancelled,
    ) {
        Ok(_) => Ok(()),
        Err(
            error @ (super::rename::RenameError::IdentityMismatch
            | super::rename::RenameError::ReparsePoint
            | super::rename::RenameError::CrossVolume
            | super::rename::RenameError::Unsupported),
        ) => Err(CopyError::GuardRejected(format!("{error:?}"))),
        Err(super::rename::RenameError::DestinationExists) => Err(CopyError::DestinationExists),
        Err(super::rename::RenameError::Cancelled) => Err(CopyError::Cancelled),
        Err(super::rename::RenameError::MutationUncertain(detail)) => Err(CopyError::Unknown(detail)),
        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
    }
}

fn move_file_guarded_with_undo(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
    on_proof: &mut dyn FnMut(super::model::MoveUndoReceipt),
) -> Result<(), CopyError> {
    move_file_guarded_with_undo_using(
        source, destination, expected_source, expected_destination_directory, cancelled, on_proof,
        |source, destination, expected_source, expected_destination_directory, cancelled, _on_proof| {
            super::rename::move_same_volume_no_replace_guarded_with_undo(
                source, destination, expected_source, expected_destination_directory, cancelled,
            )
        },
    )
}

fn move_file_guarded_with_undo_using(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
    on_proof: &mut dyn FnMut(super::model::MoveUndoReceipt),
    attempt: impl FnOnce(
        &Path,
        &Path,
        &crate::filesystem_identity::FilesystemIdentity,
        &crate::filesystem_identity::FilesystemIdentity,
        &AtomicBool,
        &mut dyn FnMut(super::model::MoveUndoReceipt),
    ) -> Result<Option<super::model::MoveUndoReceipt>, super::rename::RenameError>,
) -> Result<(), CopyError> {
    match attempt(source, destination, expected_source, expected_destination_directory, cancelled, on_proof) {
        Ok(Some(receipt)) => { on_proof(receipt); Ok(()) }
        Ok(None) => move_file_guarded(source, destination, expected_source, expected_destination_directory, cancelled),
        // 一度relative native mutationを試した後はpath経由で再試行せず、結果を保守的に返す。
        Err(super::rename::RenameError::DestinationExists) => Err(CopyError::DestinationExists),
        Err(super::rename::RenameError::Cancelled) => Err(CopyError::Cancelled),
        Err(super::rename::RenameError::MutationUncertain(detail)) => Err(CopyError::Unknown(detail)),
        Err(super::rename::RenameError::IdentityMismatch | super::rename::RenameError::ReparsePoint
            | super::rename::RenameError::CrossVolume | super::rename::RenameError::Unsupported) => Err(CopyError::GuardRejected("relative Move route changed or refused".into())),
        Err(super::rename::RenameError::Partial(detail)) => Err(CopyError::Partial(detail)),
        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
    }
}

fn should_emit_guarded_move_no_effect_proof(result: &Result<(), CopyError>) -> bool {
    matches!(result, Err(CopyError::Io(_)))
}

/// Copies and moves of a link object preserve the raw link type and target.
/// The target is only read as reparse data; it is never enumerated or opened.
fn copy_link_object(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    let kind = super::privileged::inspect_link_object(source)
        .map_err(|error| CopyError::Io(format!("{error:?}")))?;
    match super::privileged::copy_link_object_direct(source, destination) {
        Ok(_) => Ok(()),
        // Junction creation currently travels through the narrow PowerShell
        // adapter and cannot retain its raw Win32 access status.  Retrying the
        // already-classified source through the broker is safe; symbolic links
        // retain the normal permission-only retry policy.
        Err(error)
            if kind == super::privileged::LinkObjectKind::Junction
                || super::privileged::requires_elevation(&error) =>
        {
            super::privileged::session_broker()
                .copy_link_object(source, destination, || cancelled.load(Ordering::Acquire))
                .map_err(CopyError::Io)
        }
        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
    }
}

fn move_link_object(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    let kind = super::privileged::inspect_link_object(source)
        .map_err(|error| CopyError::Io(format!("{error:?}")))?;
    let result = super::privileged::move_link_object_direct(source, destination);
    let outcome = match result {
        Ok(outcome) => Ok(outcome),
        Err(error)
            if kind == super::privileged::LinkObjectKind::Junction
                || super::privileged::requires_elevation(&error) =>
        {
            super::privileged::session_broker()
                .move_link_object(source, destination, || cancelled.load(Ordering::Acquire))
        }
        Err(error) => Err(format!("{error:?}")),
    };
    match outcome {
        Ok(super::privileged::LinkMoveResult::Completed) => Ok(()),
        Ok(super::privileged::LinkMoveResult::Partial(detail)) => Err(CopyError::Partial(detail)),
        Err(detail) => Err(CopyError::Io(detail)),
    }
}

/// The elevated broker calls this exact adapter, rather than a second
/// `std::fs::rename` implementation, so same-volume identity and cross-volume
/// publish/delete rules remain identical on both sides of the IPC boundary.
pub(crate) fn move_file_without_broker(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<(), CopyError> {
    move_file_without_broker_observed(source, destination, cancelled, on_bytes, &mut || {}, super::model::ItemId(0), None, &mut |_| {}, &mut |_| {}, &mut |_| {})
}

fn move_file_without_broker_observed(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
    after_publish: &mut dyn FnMut(),
    item: super::model::ItemId,
    retry_guard: Option<&super::model::CrossVolumeMoveRetryGuard>,
    on_source_retained: &mut dyn FnMut(super::model::PublishedMoveSourceRetainedProof),
    on_disposition: &mut dyn FnMut(super::model::PublishedMoveDispositionProof),
    on_no_effect: &mut dyn FnMut(super::model::CrossVolumeMoveNoEffectProof),
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    if let Some(guard) = retry_guard {
        #[cfg(target_os = "windows")]
        { return windows::move_cross_volume_regular_file(source, destination, cancelled, on_bytes, after_publish, item, Some(guard), None, None, on_source_retained, on_disposition, on_no_effect); }
        #[cfg(not(target_os = "windows"))]
        { let _ = guard; return Err(CopyError::Unsupported); }
    }
    match super::rename::move_same_volume_no_replace(source, destination) {
        Ok(_) => Ok(()),
        Err(super::rename::RenameError::Unsupported) => {
            #[cfg(target_os = "windows")]
            {
                // The Windows adapter itself retains and validates the exact
                // source and destination-parent handles it will mutate.
                windows::move_cross_volume_regular_file(
                    source,
                    destination,
                    cancelled,
                    on_bytes,
                    after_publish,
                    item,
                    None,
                    None,
                    None,
                    on_source_retained,
                    on_disposition,
                    on_no_effect,
                )
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (on_bytes, on_no_effect);
                Err(CopyError::Unsupported)
            }
        }
        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
    }
}

/// The Windows adapters retain native error detail in a display string.  It is
/// only used to decide whether to retry the same no-replace adapter under the
/// broker; all other failures keep their original operation outcome.
fn operation_requires_elevation(result: &Result<(), CopyError>) -> bool {
    matches!(
        result,
        Err(CopyError::Io(detail))
            if detail.contains("PermissionDenied")
                || detail.contains("code: 5")
                || detail.contains("code: 1314")
    )
}

/// Rename is deliberately a separate capability from Cut/Paste move: its
/// destination is captured as one exact sibling path, never inferred again
/// from a row or a current directory after the inline editor has closed.
fn rename_file_in_parent(
    source: &Path,
    destination: &Path,
    identity_guard: Option<(
        &crate::filesystem_identity::FilesystemIdentity,
        &crate::filesystem_identity::FilesystemIdentity,
    )>,
    reparse_guard: Option<&super::model::RenameReparseGuard>,
    expected_kind: Option<crate::directory::EntryKind>,
    token: AttemptToken,
    item: ItemId,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    emit_undo_candidate: bool,
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    if let Some(guard) = reparse_guard {
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(CopyError::Unsupported)?;
        if super::rename::is_case_only_rename(source, name) {
            let Some((source_identity, parent_identity)) = identity_guard else {
                return Err(CopyError::GuardRejected(
                    "case-only rename requires accepted source and parent identities".into(),
                ));
            };
            return super::rename::rename_case_only_guarded(
                source,
                destination,
                source_identity,
                parent_identity,
                Some(guard),
                expected_kind,
                token,
                item,
                cancelled,
                controls,
                emit,
            )
            .map(|receipt| {
                if emit_undo_candidate {
                    if let Some(receipt) = receipt {
                        emit(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                    }
                }
            })
            .map_err(|error| match error {
                super::rename::RenameError::Cancelled => CopyError::Cancelled,
                super::rename::RenameError::DestinationExists => CopyError::DestinationExists,
                super::rename::RenameError::Partial(detail) => CopyError::Partial(detail),
                error => CopyError::Io(format!("{error:?}")),
            });
        }
        return super::rename::rename_reparse_sibling_guarded_for_kind(
            source,
            destination,
            guard,
            expected_kind,
            cancelled,
        )
        .map_err(|error| match error {
            super::rename::RenameError::Cancelled => CopyError::Cancelled,
            super::rename::RenameError::DestinationExists => CopyError::DestinationExists,
            super::rename::RenameError::Partial(detail) => CopyError::Partial(detail),
            error => CopyError::GuardRejected(format!("{error:?}")),
        });
    }
    if is_link_or_reparse_point(source) {
        return Err(CopyError::GuardRejected(
            "filesystem link Rename requires a no-follow accepted identity".into(),
        ));
    }
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(CopyError::Unsupported)?;
    if super::rename::is_case_only_rename(source, name) {
        let Some((source_identity, parent_identity)) = identity_guard else {
            return Err(CopyError::GuardRejected(
                "case-only rename requires accepted source and parent identities".into(),
            ));
        };
        return super::rename::rename_case_only_guarded(
            source,
            destination,
            source_identity,
            parent_identity,
            None,
            expected_kind,
            token,
            item,
            cancelled,
            controls,
            emit,
        )
        .map(|receipt| {
            if emit_undo_candidate {
                if let Some(receipt) = receipt {
                    emit(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                }
            }
        })
        .map_err(|error| match error {
            super::rename::RenameError::Cancelled => CopyError::Cancelled,
            super::rename::RenameError::DestinationExists => CopyError::DestinationExists,
            super::rename::RenameError::Partial(detail) => CopyError::Partial(detail),
            error => CopyError::Io(format!("{error:?}")),
        });
    }
    if let Some((source_identity, parent_identity)) = identity_guard {
        if emit_undo_candidate {
            match super::rename::rename_sibling_guarded_with_undo(
                source, destination, source_identity, parent_identity, cancelled,
            ) {
                Ok(Some(receipt)) if !receipt.source.eq(&receipt.destination)
                    && receipt.cross_volume_file.is_none() => {
                        emit(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                        return Ok(());
                }
                Ok(Some(_)) => return Err(CopyError::Unknown("inline rename returned an invalid Move receipt".into())),
                Ok(None) => {}
                Err(super::rename::RenameError::Cancelled) => return Err(CopyError::Cancelled),
                Err(super::rename::RenameError::DestinationExists) => return Err(CopyError::DestinationExists),
                Err(super::rename::RenameError::MutationUncertain(detail)) => return Err(CopyError::Unknown(detail)),
                Err(super::rename::RenameError::Partial(detail)) => return Err(CopyError::Partial(detail)),
                Err(error) => return Err(CopyError::GuardRejected(format!("{error:?}"))),
            }
        }
        return super::rename::rename_sibling_guarded(
            source,
            destination,
            source_identity,
            parent_identity,
            cancelled,
        )
        .map_err(|error| match error {
            super::rename::RenameError::Cancelled => CopyError::Cancelled,
            super::rename::RenameError::DestinationExists => CopyError::DestinationExists,
            error => CopyError::GuardRejected(format!("{error:?}")),
        });
    }
    super::rename::rename_in_parent(source, name)
        .map(|_| ())
        .map_err(|error| CopyError::Io(format!("{error:?}")))
}

fn move_keep_both_observed(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    mut on_destination: impl FnMut(&Path),
    on_bytes: &mut dyn FnMut(u64),
    item: super::model::ItemId,
    on_source_retained: &mut dyn FnMut(super::model::PublishedMoveSourceRetainedProof),
    on_disposition: &mut dyn FnMut(super::model::PublishedMoveDispositionProof),
    on_no_effect: &mut dyn FnMut(super::model::CrossVolumeMoveNoEffectProof),
    clipboard_destination_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
    clipboard_source: Option<&super::model::ClipboardMoveSourceEvidence>,
    on_undo: &mut dyn FnMut(super::model::MoveUndoReceipt),
) -> Result<(), CopyError> {
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let candidate = super::model::keep_both_candidate_path(destination, suffix)
            .ok_or(CopyError::Unsupported)?;
        if candidate.exists() {
            continue;
        }
        on_destination(&candidate);
        if let Some(destination_identity) = clipboard_destination_identity {
            if let Some(source_evidence) = clipboard_source {
                let undo_sink = std::cell::RefCell::new(&mut *on_undo);
                let mut emit_undo = |receipt| (undo_sink.borrow_mut())(receipt);
                let mut candidate_disposition = |proof: super::model::PublishedMoveDispositionProof| {
                    on_disposition(proof.clone());
                    if let Some(content_sha256) = proof.content_sha256.as_ref().filter(|hash|
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && proof.source_pre_disposition_signature.is_reliable_for_cross_volume_move_retry()
                            && proof.destination_signature.is_reliable_for_cross_volume_move_retry()
                    ) {
                        (undo_sink.borrow_mut())(super::model::MoveUndoReceipt {
                            source: proof.source.clone(),
                            destination: proof.destination.clone(),
                            object_identity: proof.destination_identity.clone(),
                            object_kind: super::model::MoveUndoObjectKind::RegularFile,
                            source_parent_route: proof.source_parent_route.clone(),
                            destination_parent_route: proof.destination_parent_route.clone(),
                            cross_volume_file: Some(super::model::CrossVolumeMoveUndoProof {
                                original_source_identity: proof.source_identity.clone(),
                                original_source_signature: proof.source_pre_disposition_signature,
                                destination_signature: proof.destination_signature,
                                content_sha256: content_sha256.clone(),
                                directory_manifest: None,
                            }),
                        });
                    }
                };
                match move_file_clipboard_worker_observed_with_undo(
                    source, &candidate, destination_identity, Some(source_evidence), cancelled,
                    &mut *on_bytes, item, None, on_source_retained, &mut candidate_disposition,
                    on_no_effect, &mut emit_undo,
                ) {
                    Ok(()) => return Ok(()),
                    Err(CopyError::DestinationExists) => continue,
                    Err(error) => return Err(error),
                }
            } else {
                match super::rename::move_same_volume_no_replace_worker_observed_with_undo(
                    source, &candidate, destination_identity, cancelled,
                ) {
                    Ok(Some(receipt)) => {
                        on_undo(receipt);
                        return Ok(());
                    }
                    Ok(None) | Err(super::rename::RenameError::CrossVolume | super::rename::RenameError::Unsupported) => {}
                    Err(super::rename::RenameError::DestinationExists) => continue,
                    Err(super::rename::RenameError::Cancelled) => return Err(CopyError::Cancelled),
                    Err(super::rename::RenameError::MutationUncertain(detail)) => return Err(CopyError::Unknown(detail)),
                    Err(super::rename::RenameError::Partial(detail)) => return Err(CopyError::Partial(detail)),
                    Err(error) => return Err(CopyError::Io(format!("{error:?}"))),
                }
            }
        }
        let mut candidate_disposition = |proof: super::model::PublishedMoveDispositionProof| {
            on_disposition(proof.clone());
            if let Some(content_sha256) = proof.content_sha256.as_ref().filter(|hash|
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && proof.source_pre_disposition_signature.is_reliable_for_cross_volume_move_retry()
                    && proof.destination_signature.is_reliable_for_cross_volume_move_retry()
            ) {
                on_undo(super::model::MoveUndoReceipt {
                    source: proof.source.clone(),
                    destination: proof.destination.clone(),
                    object_identity: proof.destination_identity.clone(),
                    object_kind: super::model::MoveUndoObjectKind::RegularFile,
                    source_parent_route: proof.source_parent_route.clone(),
                    destination_parent_route: proof.destination_parent_route.clone(),
                    cross_volume_file: Some(super::model::CrossVolumeMoveUndoProof {
                        original_source_identity: proof.source_identity.clone(),
                        original_source_signature: proof.source_pre_disposition_signature,
                        destination_signature: proof.destination_signature,
                        content_sha256: content_sha256.clone(),
                        directory_manifest: None,
                    }),
                });
            }
        };
        match move_file_observed(source, &candidate, cancelled, &mut *on_bytes, item, None, &mut *on_source_retained, &mut candidate_disposition, &mut *on_no_effect) {
            Ok(_) => return Ok(()),
            Err(CopyError::Io(_)) if candidate.exists() => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

fn move_keep_both_guarded(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
    mut on_destination: impl FnMut(&Path),
    on_undo: &mut dyn FnMut(super::model::MoveUndoReceipt),
) -> Result<(), CopyError> {
    let parent = destination.parent().ok_or(CopyError::Unsupported)?;
    let stem = destination
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = destination.extension().map(|value| value.to_string_lossy());
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let name = match &extension {
            Some(extension) if !extension.is_empty() => format!("{stem} ({suffix}).{extension}"),
            _ => format!("{stem} ({suffix})"),
        };
        let candidate = parent.join(name);
        if candidate.exists() {
            continue;
        }
        on_destination(&candidate);
        match move_file_guarded_with_undo(
            source,
            &candidate,
            expected_source,
            expected_destination_directory,
            cancelled,
            on_undo,
        ) {
            Ok(_) => return Ok(()),
            Err(CopyError::DestinationExists) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

#[cfg(windows)]
fn move_keep_both_cross_volume_directory(
    token: AttemptToken,
    item: ItemId,
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_parent: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    mut on_destination: impl FnMut(&Path),
    mut on_bytes: impl FnMut(u64),
    emit: &mut impl FnMut(ExecutorFact),
    emit_undo_candidate: bool,
) -> Result<(), CopyError> {
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let candidate = super::model::keep_both_candidate_path(destination, suffix)
            .ok_or(CopyError::Unsupported)?;
        if candidate.exists() {
            continue;
        }
        on_destination(&candidate);
        match windows::move_cross_volume_directory(
            token,
            item,
            source,
            &candidate,
            expected_source,
            expected_destination_parent,
            cancelled,
            controls,
            &mut on_bytes,
            emit,
        ) {
            Ok(receipt) => {
                if emit_undo_candidate {
                    emit(ExecutorFact::MoveUndoCandidate { token, item, receipt });
                }
                return Ok(());
            }
            Err(CopyError::DestinationExists) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

/// Symbolic-link creation never reads, copies, or follows the source.  The
/// direct attempt keeps Developer Mode fast; the broker path is entered only
/// for the Windows privilege errors that prove it is necessary.
fn create_symbolic_link(
    source: &Path,
    destination: &Path,
    expected_parent_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
    cancelled: &AtomicBool,
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    match super::privileged::create_symbolic_link_direct(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if super::privileged::requires_elevation(&error) => {
            super::privileged::session_broker()
                .create_symbolic_link(source, destination, expected_parent_identity, || {
                    cancelled.load(Ordering::Acquire)
                })
                .map_err(CopyError::Io)
        }
        Err(error) => Err(CopyError::Io(format!("{error:?}"))),
    }
}

/// KeepBoth uses the same no-overwrite invariant as Copy.  Each candidate is
/// created atomically by the OS, so another creator can only make us try the
/// next suffix; it can never be overwritten by a stale existence check.
fn create_symbolic_link_keep_both(
    source: &Path,
    destination: &Path,
    expected_parent_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
    cancelled: &AtomicBool,
    mut on_destination: impl FnMut(&Path),
) -> Result<(), CopyError> {
    let parent = destination.parent().ok_or(CopyError::Unsupported)?;
    let stem = destination
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = destination.extension().map(|value| value.to_string_lossy());
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let name = match extension.as_deref() {
            Some(extension) if !extension.is_empty() => format!("{stem} ({suffix}).{extension}"),
            _ => format!("{stem} ({suffix})"),
        };
        let candidate = parent.join(name);
        match create_symbolic_link(source, &candidate, expected_parent_identity, cancelled) {
            Ok(()) => {
                on_destination(&candidate);
                return Ok(());
            }
            Err(CopyError::Io(_)) if candidate.exists() => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

/// Junctions have a folder-only contract.  Checking the direct metadata before
/// the broker call makes a clipboard file a normal failed item rather than an
/// elevated request with an ambiguous Windows error.
fn create_junction(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<(), CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    let metadata = source
        .symlink_metadata()
        .map_err(|error| CopyError::Io(format!("{error:?}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CopyError::Unsupported);
    }
    match super::privileged::create_junction_direct(source, destination) {
        Ok(()) => Ok(()),
        // `New-Item -ItemType Junction` cannot preserve the raw Win32 access
        // code through PowerShell.  Source/type/collision facts have already
        // been checked above, so the same broker retry is the only safe way to
        // distinguish a protected destination from the normal process token.
        Err(_) => super::privileged::session_broker()
            .create_junction(source, destination, || cancelled.load(Ordering::Acquire))
            .map_err(CopyError::Io),
    }
}

fn create_junction_keep_both(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    mut on_destination: impl FnMut(&Path),
) -> Result<(), CopyError> {
    let parent = destination.parent().ok_or(CopyError::Unsupported)?;
    let stem = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let candidate = parent.join(format!("{stem} ({suffix})"));
        match create_junction(source, &candidate, cancelled) {
            Ok(()) => {
                on_destination(&candidate);
                return Ok(());
            }
            Err(CopyError::Io(_)) if candidate.exists() => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

fn change_hints(
    kind: OperationKind,
    sources: &[PathBuf],
    destination: Option<PathBuf>,
) -> Vec<FsChangeHint> {
    let mut directories: Vec<PathBuf> = match kind {
        OperationKind::Rename | OperationKind::BatchRename | OperationKind::BatchRenameUndo => {
            destination
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .into_iter()
                .collect()
        }
        _ => destination.into_iter().collect(),
    };
    if matches!(
        kind,
        OperationKind::Move
            | OperationKind::ExternalMoveCleanup
            | OperationKind::Trash
            | OperationKind::Rename
            | OperationKind::BatchRename
            | OperationKind::BatchRenameUndo
    ) {
        directories.extend(
            sources
                .iter()
                .filter_map(|path| path.parent().map(Path::to_path_buf)),
        );
    }
    directories.sort();
    directories.dedup();
    directories
        .into_iter()
        .map(|directory| FsChangeHint { directory })
        .collect()
}

fn has_uncertain_trash_result(kind: OperationKind, receipts: &[ItemReceipt]) -> bool {
    kind == OperationKind::Trash
        && receipts
            .iter()
            .any(|receipt| receipt.outcome == ItemOutcome::Unknown)
}

fn trash_file(
    source: &Path,
    accepted: &super::model::TrashSourceGuard,
    reparse_tag: Option<u32>,
    cancelled: &AtomicBool,
) -> Result<super::trash::TrashReceipt, CopyError> {
    trash_file_with_adapter(source, accepted, reparse_tag, cancelled, || {
        super::trash::move_to_trash_with_guard(source, accepted, reparse_tag)
    })
}

fn trash_file_with_adapter(
    _source: &Path,
    _accepted: &super::model::TrashSourceGuard,
    _reparse_tag: Option<u32>,
    cancelled: &AtomicBool,
    move_to_trash: impl FnOnce() -> Result<super::trash::TrashReceipt, super::trash::TrashError>,
) -> Result<super::trash::TrashReceipt, CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    move_to_trash().map_err(|error| match error {
        super::trash::TrashError::MutationUncertain(detail) => CopyError::Unknown(detail),
        error => CopyError::Io(format!("{error:?}")),
    })
}

fn copy_outcome(result: Result<(), CopyError>) -> (ItemOutcome, Option<String>) {
    match result {
        Ok(()) => (ItemOutcome::Completed, None),
        Err(CopyError::Cancelled) => (
            ItemOutcome::CancelledBeforeStart,
            Some("cancel requested".into()),
        ),
        Err(CopyError::Partial(detail) | CopyError::PrePublishPartial(detail)) => {
            (ItemOutcome::Partial, Some(detail))
        }
        Err(CopyError::Unknown(detail)) => (ItemOutcome::Unknown, Some(detail)),
        Err(CopyError::UnknownDirectoryPublication(evidence)) => (
            ItemOutcome::Unknown,
            Some(match serde_json::to_string(&evidence) {
                Ok(payload) => format!(
                    "Directory Copy publication is uncertain during {:?}: {}; {}{}",
                    evidence.phase,
                    evidence.detail,
                    super::model::DIRECTORY_COPY_EVIDENCE_PREFIX,
                    payload
                ),
                Err(_) => format!(
                    "Directory Copy publication is uncertain during {:?}: {}",
                    evidence.phase, evidence.detail
                ),
            }),
        ),
        Err(CopyError::DirectoryCopyStageRetained { evidence, cause }) => {
            let outcome = match *cause {
                CopyError::Cancelled => ItemOutcome::CancelledBeforeStart,
                CopyError::Partial(_) | CopyError::PrePublishPartial(_) => ItemOutcome::Partial,
                CopyError::Unknown(_) | CopyError::UnknownDirectoryPublication(_) => ItemOutcome::Unknown,
                _ => ItemOutcome::Failed,
            };
            let detail = match serde_json::to_string(&evidence) {
                Ok(payload) => format!("Directory Copy retained an unpublished stage during {:?}: {}; {}{}", evidence.phase, evidence.detail, super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX, payload),
                Err(_) => format!("Directory Copy retained an unpublished stage: {}", evidence.detail),
            };
            (outcome, Some(detail))
        }
        Err(error) => (ItemOutcome::Failed, Some(format!("{error:?}"))),
    }
}

fn item_receipt(
    kind: OperationKind,
    item: ItemId,
    source: &Path,
    destination: Option<&Path>,
    outcome: ItemOutcome,
    detail: Option<String>,
    destination_published_override: Option<bool>,
    copy_undo: Option<super::model::CopyUndoReceipt>,
) -> ItemReceipt {
    let move_receipt = matches!(kind, OperationKind::Move)
        .then(|| {
            destination.map(|destination| MoveReceipt {
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
                destination_published: destination_published_override.unwrap_or(matches!(
                    outcome,
                    ItemOutcome::Completed | ItemOutcome::Partial
                )),
                source_removed: outcome == ItemOutcome::Completed,
            })
        })
        .flatten();
    ItemReceipt {
        item,
        outcome,
        detail,
        created_folder_path: None,
        created_archive_path: None,
        move_receipt,
        replace_receipt: None,
        copy_undo,
    }
}

/// Performs detached, read-only revalidation for a pending inspection ticket.
pub(crate) fn inspect_guarded_move_candidates(
    mut work: super::model::GuardedMoveInspectionResult,
) -> super::model::GuardedMoveInspectionResult {
    for candidate in &mut work.candidates {
        #[cfg(windows)]
        {
            let (source_matches, destination_parent_matches, destination) =
                windows::inspect_guarded_move_no_effect(&candidate.proof);
            candidate.source_matches = source_matches;
            candidate.destination_parent_matches = destination_parent_matches;
            candidate.destination = destination;
        }
        #[cfg(not(windows))]
        {
            candidate.source_matches = false;
            candidate.destination_parent_matches = false;
            candidate.destination = super::model::GuardedMoveDestinationState::Indeterminate;
        }
    }
    work
}

pub(crate) fn inspect_cross_volume_move_candidates(mut work: super::model::CrossVolumeMoveInspectionResult) -> super::model::CrossVolumeMoveInspectionResult {
    for candidate in &mut work.candidates {
        #[cfg(windows)]
        {
            let observed = windows::inspect_cross_volume_move_no_effect(&candidate.proof);
            candidate.source_matches = observed.0;
            candidate.destination_parent_matches = observed.1;
            candidate.destination = observed.2;
        }
        #[cfg(not(windows))]
        {
            candidate.source_matches = false;
            candidate.destination_parent_matches = false;
            candidate.destination = super::model::GuardedMoveDestinationState::Indeterminate;
        }
    }
    work
}

pub(crate) fn inspect_published_move_candidates(
    mut work: super::model::PublishedMoveInspectionResult,
) -> super::model::PublishedMoveInspectionResult {
    for candidate in &mut work.candidates {
        #[cfg(windows)]
        {
            let observed = windows::inspect_published_move(&candidate.proof);
            candidate.source = observed.0;
            candidate.destination_parent = observed.1;
            candidate.destination = observed.2;
        }
        #[cfg(not(windows))]
        {
            candidate.source = super::model::PublishedMoveInspectionState::Indeterminate;
            candidate.destination_parent = super::model::PublishedMoveInspectionState::Indeterminate;
            candidate.destination = super::model::PublishedMoveInspectionState::Indeterminate;
        }
    }
    work
}

pub(crate) fn inspect_copy_failure_candidates(
    mut work: super::model::CopyFailureInspectionResult,
) -> super::model::CopyFailureInspectionResult {
    for candidate in &mut work.candidates {
        #[cfg(windows)]
        {
            let result = windows::inspect_copy_failure(&candidate.provenance);
            candidate.source = result.0;
            candidate.destination_parent = result.1;
            candidate.destination = result.2;
            candidate.stage = result.3;
            candidate.stage_content = super::model::CopyInspectionState::Indeterminate;
        }
        #[cfg(not(windows))]
        {
            candidate.source = super::model::CopyInspectionState::Indeterminate;
            candidate.destination_parent = super::model::CopyInspectionState::Indeterminate;
            candidate.destination = super::model::CopyInspectionState::Indeterminate;
            candidate.stage = super::model::CopyInspectionState::Indeterminate;
            candidate.stage_content = super::model::CopyInspectionState::Indeterminate;
        }
    }
    for candidate in &mut work.directory_candidates {
        #[cfg(windows)]
        {
            use super::model::{CopyDirectoryInspectionEvidence as Evidence, CopyDirectoryStagePhase};
            let (stage_path, destination_path, expected, stage_identity_is_unverified) = match &candidate.evidence {
                Evidence::Publication(e) => (&e.stage_path_units, &e.destination_path_units, &e.expected_root_identity, false),
                Evidence::RetainedStage(e) => (&e.stage_path_units, &e.destination_path_units, &e.expected_root_identity,
                    e.phase == CopyDirectoryStagePhase::CreatedRootIdentityUnverified),
            };
            let expected = (!expected.is_empty()).then_some(expected.as_str());
            candidate.stage_root = if stage_identity_is_unverified {
                super::model::CopyInspectionState::Indeterminate
            } else {
                expected.map_or(super::model::CopyInspectionState::Indeterminate,
                    |identity| windows::inspect_directory_root_identity(stage_path, Some(identity)))
            };
            candidate.destination_root = match &candidate.evidence {
                Evidence::Publication(_) => windows::inspect_directory_root_identity(destination_path, expected),
                Evidence::RetainedStage(_) => windows::inspect_directory_root_identity(destination_path, None),
            };
        }
        #[cfg(not(windows))]
        {
            candidate.stage_root = super::model::CopyInspectionState::Indeterminate;
            candidate.destination_root = super::model::CopyInspectionState::Indeterminate;
        }
    }
    work
}

pub(crate) fn inspect_directory_move_roots(
    probe: &super::model::DirectoryMoveRootProbe,
) -> super::model::DirectoryMoveRootObservations {
    use super::model::CopyInspectionState as State;
    #[cfg(windows)]
    fn observe(path: &Path, expected: Option<&str>) -> State {
        use std::os::windows::ffi::OsStrExt;
        let Some(expected) = expected else { return State::Indeterminate; };
        let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        windows::inspect_directory_root_identity(&units, Some(expected))
    }
    #[cfg(not(windows))]
    fn observe(_path: &Path, _expected: Option<&str>) -> State { State::Indeterminate }

    super::model::DirectoryMoveRootObservations {
        source: observe(&probe.source, probe.expected_source_identity.as_deref()),
        destination: observe(&probe.destination, probe.expected_destination_identity.as_deref()),
        stage: match (&probe.stage, probe.expected_stage_identity.as_deref()) {
            (Some(path), expected) => observe(path, expected),
            (None, _) => State::Indeterminate,
        },
        backup: match (&probe.backup, probe.expected_backup_identity.as_deref()) {
            (Some(path), expected) => observe(path, expected),
            (None, _) => State::Indeterminate,
        },
    }
}

pub(crate) fn inspect_reopened_directory_move_roots(
    probe: &super::model::ReopenedDirectoryMoveProbe,
) -> super::model::ReopenedDirectoryMoveObservations {
    use super::model::CopyInspectionState as State;
    #[cfg(windows)]
    fn observe(path: &Path, expected: Option<&str>) -> State {
        use std::os::windows::ffi::OsStrExt;
        let Some(expected) = expected else { return State::Indeterminate; };
        windows::inspect_directory_root_identity(&path.as_os_str().encode_wide().collect::<Vec<_>>(), Some(expected))
    }
    #[cfg(not(windows))]
    fn observe(_path: &Path, _expected: Option<&str>) -> State { State::Indeterminate }
    super::model::ReopenedDirectoryMoveObservations {
        source: observe(&probe.source, probe.expected_source_identity.as_deref()),
        destination: observe(&probe.destination, probe.expected_destination_identity.as_deref()),
    }
}

/// Fresh root-only observations for passive review of one reopened Copy stage.
pub(crate) fn inspect_reopened_copy_stage_roots(
    probe: &super::model::ReopenedCopyStageProbe,
) -> super::model::ReopenedCopyStageObservations {
    use super::model::CopyInspectionState as State;
    #[cfg(windows)]
    fn observe(path: &Path, expected: Option<&str>) -> State {
        use std::os::windows::ffi::OsStrExt;
        let Some(expected) = expected else { return State::Indeterminate; };
        windows::inspect_directory_root_identity(&path.as_os_str().encode_wide().collect::<Vec<_>>(), Some(expected))
    }
    #[cfg(not(windows))]
    fn observe(_path: &Path, _expected: Option<&str>) -> State { State::Indeterminate }
    super::model::ReopenedCopyStageObservations {
        stage: observe(&probe.stage, probe.expected_stage_identity.as_deref()),
        // No destination identity was persisted, so its identity relationship is indeterminate.
        destination: observe(&probe.destination, None),
    }
}

/// KeepBoth never chooses a name by a check alone.  Each candidate is still
/// published through the E2 no-replace primitive; an external creator only
/// makes us try the next suffix.
#[cfg(windows)]
fn copy_keep_both_regular_file(
    source: &Path,
    destination: &Path,
    token: AttemptToken,
    item: ItemId,
    cancelled: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
    on_failure: &mut dyn FnMut(super::model::CopyFailureProvenance),
    on_stage_custody: &mut dyn FnMut(super::runtime::CopyStageCustody),
) -> Result<super::model::CopyUndoReceipt, CopyError> {
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let candidate = super::model::keep_both_candidate_path(destination, suffix)
            .ok_or(CopyError::Unsupported)?;
        if candidate.exists() {
            continue;
        }

        // 各候補のstage事実は、衝突で安全に片付いた場合にserviceへ渡さない。
        // そうしないと同一itemの古い候補が後続candidateのcustodyを塞いでしまう。
        let mut candidate_failures = Vec::new();
        let mut candidate_custody = Vec::new();
        let result = windows::copy_regular_file_with_undo_observed_and_custody(
            Some(token),
            source,
            &candidate,
            cancelled,
            item,
            on_bytes,
            &mut |provenance| candidate_failures.push(provenance),
            &mut |custody| candidate_custody.push(custody),
            None,
        );
        match result {
            Ok(receipt) => return Ok(receipt),
            Err(CopyError::DestinationExists) => {
                // このerrorはadapterがowned stageを正常にdispositionした後だけ返す。
                drop(candidate_custody);
                drop(candidate_failures);
            }
            Err(error) => {
                for provenance in candidate_failures.drain(..) {
                    on_failure(provenance);
                }
                for custody in candidate_custody.drain(..) {
                    on_stage_custody(custody);
                }
                return Err(error);
            }
        }
    }
    Err(CopyError::Unsupported)
}

#[cfg(windows)]
fn copy_keep_both_directory_recovery(
    source: &Path,
    destination: &Path,
    token: AttemptToken,
    item: ItemId,
    cancelled: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
    on_file_completed: &mut dyn FnMut(),
    on_file_discovered: &mut dyn FnMut(u64, u64),
    on_directory_undo: &mut dyn FnMut(super::model::DirectoryCopyUndoReceipt),
    resolved: &mut PathBuf,
) -> Result<(), CopyError> {
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let candidate = super::model::keep_both_directory_candidate_path(destination, suffix)
            .ok_or(CopyError::Unsupported)?;
        if candidate.exists() {
            continue;
        }
        let mut candidate_undo = Vec::new();
        match windows::copy_entry_for_recovery_keep_both(
            source,
            &candidate,
            token,
            item,
            cancelled,
            on_bytes,
            on_file_completed,
            on_file_discovered,
            &mut |receipt| candidate_undo.push(receipt),
        ) {
            Ok(()) => {
                *resolved = candidate;
                for receipt in candidate_undo {
                    on_directory_undo(receipt);
                }
                return Ok(());
            }
            // copy_directory returns this only after exact staged-tree custody
            // was verified and every staged node was safely removed.
            Err(CopyError::DestinationExists) => drop(candidate_undo),
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrdinaryCopySourceKind {
    File,
    Directory,
    Indeterminate,
}

fn copy_keep_both(
    source: &Path,
    destination: &Path,
    token: AttemptToken,
    item: ItemId,
    cancelled: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
    mut on_file_completed: impl FnMut(),
    mut on_file_discovered: impl FnMut(u64, u64),
) -> Result<(), CopyError> {
    let parent = destination.parent().ok_or(CopyError::Unsupported)?;
    let stem = destination
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = destination.extension().map(|value| value.to_string_lossy());
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let name = match &extension {
            Some(extension) if !extension.is_empty() => format!("{stem} ({suffix}).{extension}"),
            _ => format!("{stem} ({suffix})"),
        };
        let candidate = parent.join(name);
        if candidate.exists() {
            continue;
        }
        match copy_entry_no_clobber(
            source,
            &candidate,
            token,
            item,
            cancelled,
            &mut on_bytes,
            &mut on_file_completed,
            &mut on_file_discovered,
        ) {
            Ok(()) => return Ok(()),
            Err(CopyError::Io(_)) if candidate.exists() => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CopyError::Unsupported)
}

#[cfg(windows)]
fn copy_keep_both_directory(
    source: &Path,
    destination: &Path,
    token: AttemptToken,
    item: ItemId,
    cancelled: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
    on_file_completed: &mut dyn FnMut(),
    on_file_discovered: &mut dyn FnMut(u64, u64),
    on_directory_undo: &mut dyn FnMut(super::model::DirectoryCopyUndoReceipt),
) -> Result<(), CopyError> {
    for suffix in 2..10_000_u32 {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let candidate = super::model::keep_both_directory_candidate_path(destination, suffix)
            .ok_or(CopyError::Unsupported)?;
        // Occupied candidates may be skipped before the adapter starts. Once it
        // starts, its error can leave a retained or ambiguous stage, so stop.
        if candidate.exists() {
            continue;
        }
        return windows::copy_entry(
            source,
            &candidate,
            token,
            item,
            cancelled,
            on_bytes,
            on_file_completed,
            on_file_discovered,
            on_directory_undo,
        );
    }
    Err(CopyError::Unsupported)
}

/// Dispatch one clipboard item to its capability-specific adapter.  A folder
/// is copied into an attempt/item-owned staging directory and appears at the
/// requested destination only after that complete tree is published.
fn copy_entry_no_clobber(
    source: &Path,
    destination: &Path,
    token: AttemptToken,
    item: ItemId,
    cancelled: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
    mut on_file_completed: impl FnMut(),
    mut on_file_discovered: impl FnMut(u64, u64),
) -> Result<(), CopyError> {
    if is_link_or_reparse_point(source) {
        return copy_link_object(source, destination, cancelled);
    }
    #[cfg(target_os = "windows")]
    {
        windows::copy_entry(
            source,
            destination,
            token,
            item,
            cancelled,
            &mut on_bytes,
            &mut on_file_completed,
            &mut on_file_discovered,
            &mut |_| {},
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (
            source,
            destination,
            token,
            item,
            cancelled,
            on_bytes,
            on_file_completed,
            on_file_discovered,
        );
        Err(CopyError::Unsupported)
    }
}

fn rejected() -> TerminalReport {
    TerminalReport {
        outcome: TerminalOutcome::Failed,
        final_counters: ProgressCounters::default(),
        receipts: Vec::new(),
        retry: RetryPlan::NotAllowed,
        change_hints: Vec::new(),
    }
}

fn seven_zip_not_implemented_report() -> TerminalReport {
    let mut report = rejected();
    report.final_counters.total_items = Some(1);
    report.final_counters.discovered_items = 1;
    report.receipts.push(super::model::ItemReceipt {
        item: super::model::ItemId(0),
        outcome: super::model::ItemOutcome::Failed,
        detail: Some("7z archive creation is not available in this operation worker".into()),
        created_folder_path: None,
        created_archive_path: None,
        move_receipt: None,
        replace_receipt: None,
        copy_undo: None,
    });
    report
}

fn seven_zip_extract_not_implemented_report(
    request: &super::model::OperationRequest,
) -> TerminalReport {
    let count = request.sources.len();
    let mut report = rejected();
    report.final_counters.total_items = Some(count as u64);
    report.final_counters.discovered_items = count as u64;
    report.final_counters.processed_items = count as u64;
    report.receipts = request
        .sources
        .iter()
        .enumerate()
        .map(|(ordinal, _)| super::model::ItemReceipt {
            item: super::model::ItemId(ordinal as u32),
            outcome: super::model::ItemOutcome::Failed,
            detail: Some("7z extraction is not available in this operation worker".into()),
            created_folder_path: None,
            created_archive_path: None,
            move_receipt: None,
            replace_receipt: None,
            copy_undo: None,
        })
        .collect();
    report
}

fn known_regular_file_bytes(sources: &[PathBuf]) -> Option<u64> {
    sources.iter().try_fold(0_u64, |total, path| {
        let metadata = path.symlink_metadata().ok()?;
        metadata
            .is_file()
            .then_some(total.checked_add(metadata.len())?)
    })
}

/// The ordered guard vector must have one row identity for every accepted Trash path.
fn trash_source_guards_are_well_formed(request: &super::model::OperationRequest) -> bool {
    if request.kind != OperationKind::Trash {
        return request.trash_source_guards.is_none();
    }
    let Some(guards) = request.trash_source_guards.as_ref() else {
        return false;
    };
    !request.sources.is_empty()
        && guards.len() == request.sources.len()
        && guards.iter().all(|guard| {
            matches!(
                guard.identity.kind,
                crate::directory::EntryKind::File | crate::directory::EntryKind::Folder
            )
        })
}

/// Recheck one leaf immediately before its Trash adapter call, after earlier batch items may
/// have blocked or yielded. This does not bind the later Shell mutation to the checked handle.
fn trash_source_identity_matches(source: &Path, accepted: &super::model::TrashSourceGuard) -> bool {
    super::trash::admit_source(source, accepted).is_ok()
}

/// Windows の `FileType::is_symlink` は junction も true として返す。さらにその外の
/// reparse point も `FILE_ATTRIBUTE_REPARSE_POINT` で止め、操作 worker が target を
/// 通常 file/folder と誤認しないようにする。
fn is_link_or_reparse_point(path: &Path) -> bool {
    let Ok(metadata) = path.symlink_metadata() else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;

        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// A Copy request has a stable item total only when every source is a regular
/// file. Folder descendants are intentionally discovered during the copy.
fn known_regular_file_count(sources: &[PathBuf]) -> Option<u64> {
    sources
        .iter()
        .all(|path| {
            path.symlink_metadata()
                .is_ok_and(|metadata| metadata.is_file())
        })
        .then_some(sources.len() as u64)
}

/// Top-level regular files are known before the worker begins. Folder contents
/// join this lower bound only when their own directory level is enumerated.
fn known_regular_file_facts(sources: &[PathBuf]) -> (u64, u64) {
    sources.iter().fold((0, 0), |(items, bytes), path| {
        let Ok(metadata) = path.symlink_metadata() else {
            return (items, bytes);
        };
        metadata
            .is_file()
            .then_some((
                items.saturating_add(1),
                bytes.saturating_add(metadata.len()),
            ))
            .unwrap_or((items, bytes))
    })
}

/// Copies one ordinary file without overwriting `destination`.
///
/// The destination directory is held without `FILE_SHARE_DELETE`, so its path
/// cannot be renamed or deleted between temp creation and no-replace publish.
pub fn copy_file_no_clobber(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> Result<(), CopyError> {
    let result = copy_file_no_clobber_without_broker(source, destination, cancelled, &mut on_bytes);
    if !operation_requires_elevation(&result) {
        return result;
    }
    super::privileged::session_broker()
        .copy_regular_file(source, destination, || cancelled.load(Ordering::Acquire))
        .map_err(CopyError::Io)
}

/// The broker uses the same no-clobber file publication primitive as the
/// ordinary worker.  Keeping the no-broker entry explicit avoids recursive IPC
/// when the elevated process encounters an unrelated I/O failure.
pub(crate) fn copy_file_no_clobber_without_broker(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<(), CopyError> {
    #[cfg(target_os = "windows")]
    {
        windows::copy(source, destination, cancelled, on_bytes)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (source, destination, cancelled, on_bytes);
        Err(CopyError::Unsupported)
    }
}

pub fn undo_copy(receipt: super::model::CopyUndoReceipt) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows::undo_copy(&receipt).map_err(|error| format!("{error:?}"))
    }
    #[cfg(not(windows))]
    {
        let _ = receipt;
        Err("Copy undo is available only on Windows".into())
    }
}

#[cfg(windows)]
pub(super) fn create_file_undo_candidate(
    parent: &Path,
    expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    name: &std::ffi::OsStr,
) -> Result<Option<super::model::CreatedObjectUndoReceipt>, CopyError> {
    windows::create_file_undo_candidate(parent, expected_parent, name)
}

#[cfg(windows)]
pub(super) fn create_folder_undo_candidate(
    parent: &Path,
    expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    name: &std::ffi::OsStr,
) -> Result<Option<super::model::CreatedObjectUndoReceipt>, CopyError> {
    windows::create_folder_undo_candidate(parent, expected_parent, name)
}

#[cfg(windows)]
#[cfg(test)]
pub(super) fn directory_identity_for_test(path: &Path) -> crate::filesystem_identity::FilesystemIdentity {
    windows::directory_identity_for_test(path).expect("test directory identity")
}

pub fn undo_created_object(receipt: super::model::CreatedObjectUndoReceipt) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows::undo_created_object(&receipt).map_err(|error| format!("{error:?}"))
    }
    #[cfg(not(windows))]
    {
        let _ = receipt;
        Err("Created-object undo is available only on Windows".into())
    }
}

struct MoveUndoFailure {
    published_restore: bool,
    message: String,
}

fn uncertain_move_undo_publication_failure(
    error: crate::operations::rename::RenameError,
    context: &str,
) -> MoveUndoFailure {
    MoveUndoFailure {
        published_restore: true,
        message: format!("{context} outcome uncertain: {error:?}; moved output retained"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryCopyUndoResult {
    Refused(String),
    Removed,
    Partial(String),
    Uncertain(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryCopyGroupUndoResult {
    Settled { consumed_reverse_suffix: usize },
    Refused { consumed_reverse_suffix: usize, error: String },
    Partial { consumed_reverse_suffix: usize, error: String },
    Uncertain { consumed_reverse_suffix: usize, error: String },
}

pub fn undo_directory_copy_group(
    receipts: Vec<super::model::DirectoryCopyUndoReceipt>,
    custody: DirectoryCopyUndoCustodySlot,
) -> DirectoryCopyGroupUndoResult {
    undo_directory_copy_group_with(receipts, |receipt| undo_directory_copy(receipt, custody.clone()))
}

pub fn undo_mixed_copy_group(
    items: Vec<super::model::CopyUndoItem>,
    custody: DirectoryCopyUndoCustodySlot,
) -> MixedCopyGroupUndoResult {
    let mut consumed = 0;
    for item in items {
        match item {
            super::model::CopyUndoItem::File(receipt) => match undo_copy(receipt) {
                Ok(()) => consumed += 1,
                Err(error) => return MixedCopyGroupUndoResult::Refused { consumed_prefix: consumed, error },
            },
            super::model::CopyUndoItem::Directory(receipt) => match undo_directory_copy(receipt, custody.clone()) {
                DirectoryCopyUndoResult::Removed => consumed += 1,
                DirectoryCopyUndoResult::Refused(error) => return MixedCopyGroupUndoResult::Refused { consumed_prefix: consumed, error },
                DirectoryCopyUndoResult::Partial(error) => return MixedCopyGroupUndoResult::Partial { consumed_prefix: consumed + 1, error },
                DirectoryCopyUndoResult::Uncertain(error) => return MixedCopyGroupUndoResult::Uncertain { consumed_prefix: consumed + 1, error },
            },
        }
    }
    MixedCopyGroupUndoResult::Settled { consumed_prefix: consumed }
}

#[derive(Debug)]
pub enum MixedCopyGroupUndoResult {
    Settled { consumed_prefix: usize },
    Refused { consumed_prefix: usize, error: String },
    Partial { consumed_prefix: usize, error: String },
    Uncertain { consumed_prefix: usize, error: String },
}

fn undo_directory_copy_group_with(
    receipts: Vec<super::model::DirectoryCopyUndoReceipt>,
    mut undo_one: impl FnMut(super::model::DirectoryCopyUndoReceipt) -> DirectoryCopyUndoResult,
) -> DirectoryCopyGroupUndoResult {
    let mut consumed = 0;
    for receipt in receipts.into_iter().rev() {
        match undo_one(receipt) {
            DirectoryCopyUndoResult::Removed => consumed += 1,
            DirectoryCopyUndoResult::Refused(error) => return DirectoryCopyGroupUndoResult::Refused { consumed_reverse_suffix: consumed, error },
            DirectoryCopyUndoResult::Partial(error) => return DirectoryCopyGroupUndoResult::Partial { consumed_reverse_suffix: consumed + 1, error },
            DirectoryCopyUndoResult::Uncertain(error) => return DirectoryCopyGroupUndoResult::Uncertain { consumed_reverse_suffix: consumed + 1, error },
        }
    }
    DirectoryCopyGroupUndoResult::Settled { consumed_reverse_suffix: consumed }
}

#[cfg(test)]
mod directory_copy_group_undo_tests {
    use super::{DirectoryCopyGroupUndoResult as GroupResult, DirectoryCopyUndoResult as ItemResult};
    use super::*;
    use std::path::PathBuf;

    fn receipts() -> Vec<super::super::model::DirectoryCopyUndoReceipt> {
        ["a", "b", "c"].into_iter().map(|name| super::super::model::DirectoryCopyUndoReceipt {
            destination: PathBuf::from(name), destination_parent_route: Vec::new(),
            root_identity: name.into(), entries: Vec::new(),
        }).collect()
    }

    #[test]
    fn refusal_consumes_only_the_already_removed_reverse_suffix() {
        let mut visited = Vec::new();
        let result = undo_directory_copy_group_with(receipts(), |receipt| {
            visited.push(receipt.destination);
            match visited.len() {
                1 => ItemResult::Removed,
                2 => ItemResult::Refused("injected no-effect refusal".into()),
                _ => panic!("an earlier item must not start after refusal"),
            }
        });
        assert_eq!(visited, vec![PathBuf::from("c"), PathBuf::from("b")]);
        assert_eq!(result, GroupResult::Refused { consumed_reverse_suffix: 1, error: "injected no-effect refusal".into() });
    }

    #[test]
    fn partial_and_uncertain_consume_the_stop_item_and_never_start_earlier_items() {
        for (stop, expected) in [
            (ItemResult::Partial("injected partial".into()), GroupResult::Partial { consumed_reverse_suffix: 2, error: "injected partial".into() }),
            (ItemResult::Uncertain("injected uncertain".into()), GroupResult::Uncertain { consumed_reverse_suffix: 2, error: "injected uncertain".into() }),
        ] {
            let mut visited = Vec::new();
            let result = undo_directory_copy_group_with(receipts(), |receipt| {
                visited.push(receipt.destination);
                if visited.len() == 1 { ItemResult::Removed } else if visited.len() == 2 { stop.clone() }
                else { panic!("an earlier item must not start after an effectful stop") }
            });
            assert_eq!(visited, vec![PathBuf::from("c"), PathBuf::from("b")]);
            assert_eq!(result, expected);
        }
    }
}

pub fn undo_directory_copy(
    receipt: super::model::DirectoryCopyUndoReceipt,
    custody: DirectoryCopyUndoCustodySlot,
) -> DirectoryCopyUndoResult {
    #[cfg(windows)]
    { windows::undo_directory_copy(&receipt, custody) }
    #[cfg(not(windows))]
    { let _ = (receipt, custody); DirectoryCopyUndoResult::Refused("directory Copy Undo is available only on Windows".into()) }
}

fn undo_move_with_effect(receipt: &super::model::MoveUndoReceipt) -> Result<(), MoveUndoFailure> {
    if receipt.cross_volume_file.is_some() {
        #[cfg(windows)]
        { windows::undo_cross_volume_move(receipt).map_err(|error| MoveUndoFailure { published_restore: error.published_restore, message: format!("{}", error.message) }) }
        #[cfg(not(windows))]
        { Err(MoveUndoFailure { published_restore: false, message: "Cross-volume Move Undo is available only on Windows".into() }) }
    } else {
        super::rename::undo_move_no_replace(receipt)
            .map_err(|error| MoveUndoFailure { published_restore: false, message: format!("{error:?}") })
    }
}

pub fn undo_move(receipt: super::model::MoveUndoReceipt) -> Result<(), String> {
    undo_move_with_effect(&receipt).map_err(|failure| failure.message)
}

pub fn undo_moves_reverse(receipts: Vec<super::model::MoveUndoReceipt>) -> (usize, bool, Result<(), String>) {
    let mut restored = 0;
    for receipt in receipts.into_iter().rev() {
        if let Err(failure) = undo_move_with_effect(&receipt) {
            if failure.published_restore {
                // v5 は戻し先の公開後に移動先削除が失敗しても履歴を再試行しない。
                // 同じ v6 receipt を残すと二重公開になるため、この項目は消費する。
                restored += 1;
            }
            return (restored, failure.published_restore, Err(failure.message));
        }
        restored += 1;
    }
    (restored, false, Ok(()))
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    #[cfg(test)]
    thread_local! {
        static COPY_TEST_FAIL_CLEANUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static COPY_TEST_FAIL_RECEIPT_FACTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static COPY_DIRECTORY_POSTPUBLISH_TEST_FAULT: std::cell::Cell<Option<CopyDirectoryPostPublishTestFault>> = const { std::cell::Cell::new(None) };
        static COPY_REPLACE_BACKUP_CLEANUP_TEST_FAILURE: std::cell::Cell<Option<(AttemptToken, ItemId)>> = const { std::cell::Cell::new(None) };
        static COPY_REPLACE_BACKUP_CHILD_CLEANUP_TEST_FAILURE: std::cell::RefCell<Option<(AttemptToken, ItemId, super::super::journal::JournalCopyReplaceChildIntent, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_POSTPUBLISH_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_PREVERIFY_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_STAGING_FAILURE_TEST: std::cell::RefCell<Option<(AttemptToken, ItemId, std::rc::Rc<std::cell::Cell<bool>>)>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_STAGE_WRITE_LOCK_TEST: std::cell::RefCell<Option<(AttemptToken, ItemId, std::rc::Rc<std::cell::Cell<bool>>, Box<dyn FnOnce(&std::fs::File)>)>> = const { std::cell::RefCell::new(None) };
        pub(super) static COPY_DIRECTORY_STAGE_WRITE_ERROR_TEST: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
        static COPY_DIRECTORY_NAMESPACE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce(&[Vec<u16>], &[Vec<u16>], &DirectoryLock)>>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_SCAN_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&[Vec<u16>]) -> bool>>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_SOURCE_OPEN_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&Path)>>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_HELD_SOURCE_FILE_TEST_HOOK: std::cell::RefCell<Option<(PathBuf, std::fs::File)>> = const { std::cell::RefCell::new(None) };
        static COPY_DIRECTORY_FAIL_STAGE_ROOT_IDENTITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static CROSS_VOLUME_MOVE_TEST_FAIL_STAGE_CREATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static CROSS_VOLUME_MOVE_TEST_FAIL_SOURCE_DISPOSITION_PATH: std::cell::RefCell<Option<(Option<AttemptToken>, Option<ItemId>, PathBuf)>> = const { std::cell::RefCell::new(None) };
        static CROSS_VOLUME_MOVE_TEST_ACTIVE_TOKEN: std::cell::Cell<Option<AttemptToken>> = const { std::cell::Cell::new(None) };
        static DIRECTORY_MOVE_CHILD_DISPOSITION_TEST_FAILURE: std::cell::RefCell<Option<(AttemptToken, u32, Vec<Vec<u16>>, std::rc::Rc<std::cell::Cell<bool>>)>> = const { std::cell::RefCell::new(None) };
        static CROSS_VOLUME_MOVE_UNDO_TEST_FAIL_DESTINATION_DELETE_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
        static CROSS_VOLUME_MOVE_UNDO_TEST_PROBE_RESTORED_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static CROSS_VOLUME_DIRECTORY_UNDO_BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnMut(&Path) -> bool>>> = const { std::cell::RefCell::new(None) };
        static CROSS_VOLUME_DIRECTORY_UNDO_ACCESS_DENIED_CANDIDATE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    #[derive(Clone, Copy)]
    pub(super) enum CopyDirectoryPostPublishTestFault {
        Cancel,
        AddForeignChild,
    }

    #[cfg(test)]
    pub(super) fn set_copy_directory_postpublish_fault_for_test(
        fault: CopyDirectoryPostPublishTestFault,
    ) {
        COPY_DIRECTORY_POSTPUBLISH_TEST_FAULT.with(|slot| slot.set(Some(fault)));
    }

    #[cfg(test)]
    pub(super) fn arm_copy_directory_foreign_child_for_test() {
        set_copy_directory_postpublish_fault_for_test(
            CopyDirectoryPostPublishTestFault::AddForeignChild,
        );
    }

    #[cfg(test)]
    pub(super) fn arm_copy_replace_backup_cleanup_failure_for_test(
        token: AttemptToken,
        item: ItemId,
    ) {
        COPY_REPLACE_BACKUP_CLEANUP_TEST_FAILURE.with(|slot| {
            assert!(slot.replace(Some((token, item))).is_none());
        });
    }

    #[cfg(test)]
    pub(super) fn arm_copy_replace_backup_child_cleanup_failure_for_test(
        token: AttemptToken,
        item: ItemId,
        intent: super::super::journal::JournalCopyReplaceChildIntent,
        durable_ack_gate: std::sync::mpsc::Receiver<()>,
    ) {
        COPY_REPLACE_BACKUP_CHILD_CLEANUP_TEST_FAILURE.with(|slot| {
            assert!(slot.replace(Some((token, item, intent, durable_ack_gate))).is_none());
        });
    }

    #[cfg(test)]
    fn fail_copy_replace_backup_child_cleanup_for_test(
        token: AttemptToken,
        item: ItemId,
        intent: &super::super::journal::JournalCopyReplaceChildIntent,
    ) -> bool {
        COPY_REPLACE_BACKUP_CHILD_CLEANUP_TEST_FAILURE.with(|slot| {
            let matches = slot.borrow().as_ref().is_some_and(|(expected_token, expected_item, expected_intent, _)|
                *expected_token == token && *expected_item == item && expected_intent == intent)
            ;
            if !matches { return false; }
            let (_, _, _, durable_ack_gate) = slot.borrow_mut().take().expect("matched cleanup fault remains armed");
            durable_ack_gate.recv().expect("test observes exact durable child intent before injected disposition failure");
            true
        })
    }

    #[cfg(test)]
    fn fail_copy_replace_backup_cleanup_for_test(token: AttemptToken, item: ItemId) -> bool {
        COPY_REPLACE_BACKUP_CLEANUP_TEST_FAILURE.with(|slot| {
            if slot.get() == Some((token, item)) {
                slot.set(None);
                true
            } else {
                false
            }
        })
    }

    #[cfg(test)]
    pub(super) fn set_copy_directory_preverify_hook_for_test(
        hook: impl FnOnce(&Path) + 'static,
    ) {
        COPY_DIRECTORY_PREVERIFY_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
    }

    #[cfg(test)]
    pub(super) fn set_copy_directory_postpublish_hook_for_test(
        hook: impl FnOnce(&Path) + 'static,
    ) {
        COPY_DIRECTORY_POSTPUBLISH_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
    }

    #[cfg(test)]
    pub(super) fn set_copy_directory_scan_hook_for_test(
        hook: impl FnMut(&[Vec<u16>]) -> bool + 'static,
    ) {
        COPY_DIRECTORY_SCAN_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
    }

    #[cfg(test)]
    pub(super) fn set_copy_directory_source_open_hook_for_test(hook: impl FnMut(&Path) + 'static) {
        COPY_DIRECTORY_SOURCE_OPEN_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
    }

    #[cfg(test)]
    pub(super) fn set_copy_directory_namespace_hook_for_test(
        hook: impl FnOnce(&[Vec<u16>], &[Vec<u16>], &DirectoryLock) + 'static,
    ) {
        COPY_DIRECTORY_NAMESPACE_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
    }

    #[cfg(test)]
    pub(super) fn fail_next_copy_directory_stage_root_identity_for_test() {
        COPY_DIRECTORY_FAIL_STAGE_ROOT_IDENTITY.with(|slot| slot.set(true));
    }

    #[cfg(test)]
    pub(super) fn set_copy_test_faults(fail_cleanup: bool, fail_receipt_facts: bool) {
        COPY_TEST_FAIL_CLEANUP.with(|value| value.set(fail_cleanup));
        COPY_TEST_FAIL_RECEIPT_FACTS.with(|value| value.set(fail_receipt_facts));
    }

    #[cfg(test)]
    pub(super) fn with_copy_test_faults_for_test<R>(
        fail_cleanup: bool,
        fail_receipt_facts: bool,
        run: impl FnOnce() -> R,
    ) -> (R, bool) {
        struct Restore {
            cleanup: bool,
            receipt_facts: bool,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                COPY_TEST_FAIL_CLEANUP.with(|value| value.set(self.cleanup));
                COPY_TEST_FAIL_RECEIPT_FACTS.with(|value| value.set(self.receipt_facts));
            }
        }
        let previous = Restore {
            cleanup: COPY_TEST_FAIL_CLEANUP.with(|value| value.replace(fail_cleanup)),
            receipt_facts: COPY_TEST_FAIL_RECEIPT_FACTS.with(|value| value.replace(fail_receipt_facts)),
        };
        let result = run();
        let cleanup_failure_consumed = !COPY_TEST_FAIL_CLEANUP.with(|value| value.get());
        drop(previous);
        (result, cleanup_failure_consumed)
    }

    #[cfg(test)]
    pub(super) fn fail_next_cross_volume_move_stage_create_for_test() {
        CROSS_VOLUME_MOVE_TEST_FAIL_STAGE_CREATE.with(|value| value.set(true));
    }

    #[cfg(test)]
    pub(super) fn fail_cross_volume_move_source_disposition_for_path_for_test(path: &Path) {
        // Legacy executor selectors use the path-only arm. Shell integration
        // coverage uses the exact AttemptToken/ItemId arm below.
        CROSS_VOLUME_MOVE_TEST_FAIL_SOURCE_DISPOSITION_PATH.with(|value| {
            *value.borrow_mut() = Some((None, None, path.to_path_buf()));
        });
    }

    #[cfg(test)]
    pub(super) fn with_cross_volume_move_source_disposition_failure_for_exact_item_for_test<R>(
        token: AttemptToken,
        item: ItemId,
        path: &Path,
        run: impl FnOnce() -> R,
    ) -> R {
        struct Restore(Option<AttemptToken>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CROSS_VOLUME_MOVE_TEST_ACTIVE_TOKEN.with(|active| active.set(self.0));
            }
        }
        let previous = CROSS_VOLUME_MOVE_TEST_ACTIVE_TOKEN.with(|active| active.replace(Some(token)));
        let _restore = Restore(previous);
        CROSS_VOLUME_MOVE_TEST_FAIL_SOURCE_DISPOSITION_PATH.with(|value| {
            *value.borrow_mut() = Some((Some(token), Some(item), path.to_path_buf()));
        });
        run()
    }

    #[cfg(test)]
    pub(super) fn with_directory_move_child_disposition_failure_for_test<R>(
        token: AttemptToken,
        item: u32,
        relative_components: Vec<Vec<u16>>,
        run: impl FnOnce() -> R,
    ) -> (R, bool) {
        struct Restore(Option<(AttemptToken, u32, Vec<Vec<u16>>, std::rc::Rc<std::cell::Cell<bool>>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                DIRECTORY_MOVE_CHILD_DISPOSITION_TEST_FAILURE.with(|slot| {
                    *slot.borrow_mut() = self.0.take();
                });
            }
        }
        let consumed = std::rc::Rc::new(std::cell::Cell::new(false));
        let previous = DIRECTORY_MOVE_CHILD_DISPOSITION_TEST_FAILURE.with(|slot| {
            slot.borrow_mut().replace((token, item, relative_components, consumed.clone()))
        });
        let _restore = Restore(previous);
        let result = run();
        (result, consumed.get())
    }

    #[cfg(test)]
    fn take_directory_move_child_disposition_failure_for_test(
        token: AttemptToken,
        item: u32,
        relative_components: &[Vec<u16>],
    ) -> bool {
        DIRECTORY_MOVE_CHILD_DISPOSITION_TEST_FAILURE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|(expected_token, expected_item, expected_path, _)| {
                *expected_token == token && *expected_item == item && expected_path.as_slice() == relative_components
            }) {
                if let Some((_, _, _, consumed)) = slot.take() {
                    consumed.set(true);
                }
                true
            } else {
                false
            }
        })
    }

    #[cfg(test)]
    fn take_cross_volume_move_source_disposition_failure_for_test(item: ItemId, path: &Path) -> bool {
        CROSS_VOLUME_MOVE_TEST_FAIL_SOURCE_DISPOSITION_PATH.with(|value| {
            let mut value = value.borrow_mut();
            if value.as_ref().is_some_and(|(expected_token, expected_item, expected_path)| {
                let active_token = CROSS_VOLUME_MOVE_TEST_ACTIVE_TOKEN.with(|active| active.get());
                expected_token.map_or(true, |expected_token| Some(expected_token) == active_token)
                    && expected_item.map_or(true, |expected_item| expected_item == item)
                    && expected_path == path
            }) {
                value.take();
                true
            } else {
                false
            }
        })
    }

    #[cfg(test)]
    fn take_cross_volume_move_stage_create_failure_for_test() -> bool {
        CROSS_VOLUME_MOVE_TEST_FAIL_STAGE_CREATE.with(|value| value.replace(false))
    }
    #[cfg(test)]
    pub(super) fn fail_next_cross_volume_move_undo_destination_delete_for_test() {
        fail_cross_volume_move_undo_destination_delete_after_successes_for_test(0);
    }
    #[cfg(test)]
    pub(super) fn fail_cross_volume_move_undo_destination_delete_after_successes_for_test(successes: usize) {
        CROSS_VOLUME_MOVE_UNDO_TEST_FAIL_DESTINATION_DELETE_AFTER.with(|value| value.set(Some(successes)));
    }
    #[cfg(test)]
    fn take_cross_volume_move_undo_destination_delete_failure_for_test() -> bool {
        CROSS_VOLUME_MOVE_UNDO_TEST_FAIL_DESTINATION_DELETE_AFTER.with(|value| match value.get() {
            Some(0) => { value.set(None); true }
            Some(successes) => { value.set(Some(successes - 1)); false }
            None => false,
        })
    }
    #[cfg(test)]
    pub(super) fn probe_cross_volume_move_undo_restored_write_for_test() {
        CROSS_VOLUME_MOVE_UNDO_TEST_PROBE_RESTORED_WRITE.with(|value| value.set(true));
    }
    #[cfg(test)]
    pub(super) fn set_cross_volume_directory_undo_before_publish_hook_for_test(hook: impl FnMut(&Path) -> bool + 'static) {
        CROSS_VOLUME_DIRECTORY_UNDO_BEFORE_PUBLISH.with(|value| *value.borrow_mut() = Some(Box::new(hook)));
    }
    #[cfg(test)]
    pub(super) fn deny_cross_volume_directory_undo_publish_for_test(candidate: &Path) {
        CROSS_VOLUME_DIRECTORY_UNDO_ACCESS_DENIED_CANDIDATE.with(|value| *value.borrow_mut() = Some(candidate.to_path_buf()));
    }
    #[cfg(test)]
    fn take_cross_volume_directory_undo_access_denied_for_test(candidate: &Path) -> bool {
        CROSS_VOLUME_DIRECTORY_UNDO_ACCESS_DENIED_CANDIDATE.with(|value| {
            let mut slot = value.borrow_mut();
            if slot.as_deref() == Some(candidate) {
                slot.take();
                true
            } else {
                false
            }
        })
    }
    #[cfg(test)]
    fn run_cross_volume_directory_undo_before_publish_hook_for_test(path: &Path) {
        CROSS_VOLUME_DIRECTORY_UNDO_BEFORE_PUBLISH.with(|value| {
            let mut slot = value.borrow_mut();
            if slot.as_mut().is_some_and(|hook| hook(path)) { slot.take(); }
        });
    }
    #[cfg(test)]
    fn take_cross_volume_move_undo_restored_write_probe_for_test() -> bool {
        CROSS_VOLUME_MOVE_UNDO_TEST_PROBE_RESTORED_WRITE.with(|value| value.replace(false))
    }
    pub(super) mod extract_cleanup;
    pub(super) mod retained_tree;
    pub(super) mod spool_tree;
    pub(super) mod shortcut;
    use ::windows::{
        Win32::{
            Foundation::{
                BOOLEAN, CloseHandle, ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, HANDLE,
            },
            Storage::FileSystem::{
                CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
                FILE_BASIC_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS,
                FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE,
                FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
                FileBasicInfo, FileDispositionInfo, FileFullDirectoryInfo,
                FileFullDirectoryRestartInfo, FileRenameInfo, FileStandardInfo, FlushFileBuffers,
                GetDriveTypeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
                GetVolumeInformationW, OPEN_EXISTING, SetFileInformationByHandle,
            },
        },
        core::{HRESULT, PCWSTR},
    };
    use std::{
        ffi::c_void,
        mem::{offset_of, size_of},
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    };

    pub(super) struct DirectoryLock(pub(super) ::windows::Win32::Foundation::HANDLE);
    impl Drop for DirectoryLock {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Enumerate through the exact held directory handle. The restart class
    /// starts a fresh scan; subsequent calls continue the same handle cursor.
    fn enumerate_directory_handle(
        directory: &DirectoryLock,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<Vec<(Vec<u16>, bool)>, CopyError> {
        enumerate_directory_handle_with_cancel(directory, max_entries, max_name_bytes, None)
    }

    fn enumerate_directory_handle_with_cancel(
        directory: &DirectoryLock,
        max_entries: usize,
        max_name_bytes: usize,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Vec<(Vec<u16>, bool)>, CopyError> {
        let mut output = Vec::new();
        let mut total_name_bytes = 0_usize;
        let mut buffer = vec![0_u64; 8 * 1024];
        let buffer_bytes =
            u32::try_from(buffer.len() * size_of::<u64>()).map_err(|_| CopyError::Unsupported)?;
        let mut class: FILE_INFO_BY_HANDLE_CLASS = FileFullDirectoryRestartInfo;
        loop {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Err(CopyError::Cancelled);
            }
            let result = unsafe {
                GetFileInformationByHandleEx(
                    directory.0,
                    class,
                    buffer.as_mut_ptr().cast::<c_void>(),
                    buffer_bytes,
                )
            };
            if let Err(error) = result {
                if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                    break;
                }
                return Err(io(error));
            }
            class = FileFullDirectoryInfo;
            let bytes = buffer.len() * size_of::<u64>();
            let mut offset = 0_usize;
            loop {
                if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    return Err(CopyError::Cancelled);
                }
                if offset + offset_of!(FILE_FULL_DIR_INFO, FileName) > bytes {
                    return Err(CopyError::Unsupported);
                }
                let entry = unsafe {
                    &*buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(offset)
                        .cast::<FILE_FULL_DIR_INFO>()
                };
                let name_bytes =
                    usize::try_from(entry.FileNameLength).map_err(|_| CopyError::Unsupported)?;
                let name_start = offset + offset_of!(FILE_FULL_DIR_INFO, FileName);
                if name_bytes % size_of::<u16>() != 0 || name_start + name_bytes > bytes {
                    return Err(CopyError::Unsupported);
                }
                let name = unsafe {
                    std::slice::from_raw_parts(
                        entry.FileName.as_ptr(),
                        name_bytes / size_of::<u16>(),
                    )
                };
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    if name.is_empty()
                        || name.contains(&0)
                        || entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                    {
                        return Err(CopyError::Unsupported);
                    }
                    total_name_bytes = total_name_bytes
                        .checked_add(name_bytes)
                        .ok_or(CopyError::Unsupported)?;
                    if output.len() >= max_entries || total_name_bytes > max_name_bytes {
                        return Err(CopyError::Unsupported);
                    }
                    output.push((
                        name.to_vec(),
                        entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                    ));
                }
                if entry.NextEntryOffset == 0 {
                    break;
                }
                let step =
                    usize::try_from(entry.NextEntryOffset).map_err(|_| CopyError::Unsupported)?;
                if step < size_of::<u32>() || offset + step <= offset || offset + step >= bytes {
                    return Err(CopyError::Unsupported);
                }
                offset += step;
            }
        }
        Ok(output)
    }

    const SOURCE_TREE_MAX_ENTRIES: usize = 1_024;
    const SOURCE_TREE_MAX_DEPTH: usize = 128;
    const SOURCE_TREE_MAX_NAME_BYTES: usize = 2 * 1024 * 1024;
    const SOURCE_TREE_MAX_RECORD_BYTES: usize = 3 * 1024 * 1024;

    #[derive(Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) struct PinnedSourceEntry {
        pub(super) relative_components: Vec<Vec<u16>>,
        pub(super) node_kind: crate::operations::journal::JournalDirectoryNodeKind,
        pub(super) source_identity: String,
        pub(super) content_fact: String,
        #[serde(skip)]
        pub(super) handle_index: usize,
    }

    pub(super) enum PinnedSourceHandle {
        Directory(DirectoryLock),
        File(std::fs::File),
    }

    /// A test-only preflight result. Keeping this value alive keeps every
    /// captured source node and path ancestor pinned until its Drop.
    pub(super) struct PinnedSourceTree {
        pub(super) entries: Vec<PinnedSourceEntry>,
        pub(super) root_identity: String,
        pub(super) root_handle_index: usize,
        pub(super) _handles: Vec<Option<PinnedSourceHandle>>,
    }

    pub(super) struct VerifiedStagedTree {
        pub(super) root_path: PathBuf,
        pub(super) root_identity: String,
        pub(super) root_chain: Vec<DirectoryLock>,
        /// Exact root handle has DELETE access for the later no-replace publish step.
        pub(super) root: DirectoryLock,
        pub(super) nodes: Vec<StagedChildFact>,
        /// Exact child handles remain pinned until publish or prepublish cleanup.
        pub(super) handles: Vec<VerifiedStagedNode>,
    }

    /// The caller keeps the source pins alive across the durable journal barrier.
    pub(super) fn register_manifest_then_publish(
        token: AttemptToken,
        item: ItemId,
        source_path: &Path,
        destination_path: &Path,
        pinned: &PinnedSourceTree,
        staged: VerifiedStagedTree,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<VerifiedPublishedTree, PublishTreeFailure> {
        register_manifest_publish_with_ledger(
            token,
            item,
            source_path,
            destination_path,
            pinned,
            staged,
            cancelled,
            controls,
            emit,
        )
        .map(|(tree, _ledger)| tree)
    }

    fn build_directory_move_replace_ledger(
        item: ItemId,
        source_path: &Path,
        destination_path: &Path,
        stage_path: &Path,
        backup_path: &Path,
        destination_parent_identity: &str,
        source: &PinnedSourceTree,
        staged: &VerifiedStagedTree,
        target: &PinnedSourceTree,
    ) -> Result<super::super::journal::JournalDirectoryMove, CopyError> {
        use super::super::journal::{
            DirectoryMoveReplacePhase, JournalDirectoryChild, JournalDirectoryMove,
            JournalDirectoryMoveReplace, JournalDirectoryMoveReplaceChild, JournalDirectoryRoot,
            JournalPath,
        };
        if item.0 != 0
            || !source_path.is_absolute()
            || !destination_path.is_absolute()
            || stage_path.parent() != destination_path.parent()
            || backup_path.parent() != destination_path.parent()
            || stage_path == destination_path
            || backup_path == destination_path
            || stage_path == backup_path
            || staged.nodes.len() != source.entries.len()
            || destination_parent_identity.is_empty()
            || target.root_identity.is_empty()
        {
            return Err(CopyError::Unsupported);
        }

        let mut source_children = Vec::with_capacity(source.entries.len());
        for (ordinal, source_entry) in source.entries.iter().enumerate() {
            let staged_entry = staged
                .nodes
                .iter()
                .find(|entry| {
                    entry.relative_components == source_entry.relative_components
                        && entry.node_kind == source_entry.node_kind
                })
                .ok_or(CopyError::Unsupported)?;
            source_children.push(JournalDirectoryChild {
                ordinal: u32::try_from(ordinal).map_err(|_| CopyError::Unsupported)?,
                node_kind: source_entry.node_kind,
                relative_components: source_entry.relative_components.clone(),
                source_identity: source_entry.source_identity.clone(),
                destination_identity: staged_entry.identity.clone(),
                content_fact: source_entry.content_fact.clone(),
                disposition_recorded: false,
            });
        }

        let mut backup_children = target
            .entries
            .iter()
            .map(|entry| JournalDirectoryMoveReplaceChild {
                ordinal: 0,
                node_kind: entry.node_kind,
                relative_components: entry.relative_components.clone(),
                identity: entry.source_identity.clone(),
                content_fact: entry.content_fact.clone(),
                disposition_recorded: false,
            })
            .collect::<Vec<_>>();
        backup_children.sort_by(|left, right| {
            right
                .relative_components
                .len()
                .cmp(&left.relative_components.len())
                .then_with(|| left.relative_components.cmp(&right.relative_components))
        });
        for (ordinal, child) in backup_children.iter_mut().enumerate() {
            child.ordinal = u32::try_from(ordinal).map_err(|_| CopyError::Unsupported)?;
        }

        let replace = JournalDirectoryMoveReplace {
            stage: JournalPath::from_path(stage_path),
            backup: JournalPath::from_path(backup_path),
            stage_identity: staged.root_identity.clone(),
            approved_target_identity: target.root_identity.clone(),
            destination_parent_identity: destination_parent_identity.to_owned(),
            backup_root_identity: target.root_identity.clone(),
            backup_children,
            backup_pending: None,
            backup_root_disposition_recorded: false,
            backup_cleanup_complete: false,
            phase: DirectoryMoveReplacePhase::Staged,
        };
        let ledger = JournalDirectoryMove {
            item: item.0,
            source: JournalPath::from_path(source_path),
            destination: JournalPath::from_path(destination_path),
            children: source_children,
            pending: None,
            root: Some(JournalDirectoryRoot {
                source_identity: source.root_identity.clone(),
                destination_identity: staged.root_identity.clone(),
                pending: None,
                disposition_recorded: false,
            }),
            cleanup_complete: false,
            replace: Some(replace),
        };
        if serde_json::to_vec(&ledger).map_err(io)?.len() > 3 * 1024 * 1024 {
            return Err(CopyError::Unsupported);
        }
        Ok(ledger)
    }

    fn journal_directory_move_replace_phase(
        token: AttemptToken,
        item: ItemId,
        phase: super::super::journal::DirectoryMoveReplacePhase,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<(), CopyError> {
        emit(ExecutorFact::DirectoryMoveReplacePhaseIntent { token, item, phase });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplacePhaseJournaledExact {
                token: ack_token,
                item: ack_item,
                phase: ack_phase,
            }) if ack_token == token
                && ack_item == item
                && ack_phase == phase
                && (phase == super::super::journal::DirectoryMoveReplacePhase::StageDiscarded
                    || !cancelled.load(Ordering::Acquire)) =>
            {
                Ok(())
            }
            Ok(WorkerControl::DirectoryMoveReplacePhaseJournalFailedExact {
                token: ack_token,
                item: ack_item,
                phase: ack_phase,
            }) if ack_token == token && ack_item == item && ack_phase == phase => {
                Err(CopyError::Partial(format!(
                    "directory Move Replace phase {phase:?} was not durably acknowledged"
                )))
            }
            _ => Err(CopyError::Cancelled),
        }
    }

    fn register_manifest_publish_with_ledger(
        token: AttemptToken,
        item: ItemId,
        source_path: &Path,
        destination_path: &Path,
        pinned: &PinnedSourceTree,
        staged: VerifiedStagedTree,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<
        (
            VerifiedPublishedTree,
            super::super::journal::JournalDirectoryMove,
        ),
        PublishTreeFailure,
    > {
        use super::super::journal::{
            JournalDirectoryChild, JournalDirectoryMove, JournalDirectoryRoot, JournalPath,
        };
        let ledger_result = (|| -> Result<JournalDirectoryMove, CopyError> {
            if cancelled.load(Ordering::Acquire)
                || staged.root_path.parent() != destination_path.parent()
                || !source_path.is_absolute()
                || !destination_path.is_absolute()
                || staged.nodes.len() != pinned.entries.len()
            {
                return Err(CopyError::Unsupported);
            }
            let mut children = Vec::with_capacity(staged.nodes.len());
            for (ordinal, source) in pinned.entries.iter().enumerate() {
                let Some(destination) = staged.nodes.iter().find(|destination| {
                    destination.relative_components == source.relative_components
                        && destination.node_kind == source.node_kind
                }) else {
                    return Err(CopyError::Unsupported);
                };
                children.push(JournalDirectoryChild {
                    ordinal: u32::try_from(ordinal).map_err(|_| CopyError::Unsupported)?,
                    node_kind: source.node_kind,
                    relative_components: source.relative_components.clone(),
                    source_identity: source.source_identity.clone(),
                    destination_identity: destination.identity.clone(),
                    content_fact: source.content_fact.clone(),
                    disposition_recorded: false,
                });
            }
            let ledger = JournalDirectoryMove {
                item: item.0,
                source: JournalPath::from_path(source_path),
                destination: JournalPath::from_path(destination_path),
                children,
                pending: None,
                root: Some(JournalDirectoryRoot {
                    source_identity: pinned.root_identity.clone(),
                    destination_identity: staged.root_identity.clone(),
                    pending: None,
                    disposition_recorded: false,
                }),
                cleanup_complete: false,
                replace: None,
            };
            if serde_json::to_vec(&ledger).map_err(io)?.len() > 3 * 1024 * 1024 {
                return Err(CopyError::Unsupported);
            }
            Ok(ledger)
        })();
        let ledger = match ledger_result {
            Ok(ledger) => ledger,
            Err(error) => {
                return Err(clean_staged_failure(staged, error));
            }
        };
        emit(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: ledger.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveManifestJournaledExact {
                token: ack_token,
                ledger: ack_ledger,
            }) if ack_token == token
                && ack_ledger == ledger
                && !cancelled.load(Ordering::Acquire) => {}
            _ => return Err(clean_staged_failure(staged, CopyError::Cancelled)),
        }
        publish_and_verify_staged_tree(staged, destination_path, cancelled)
            .map(|tree| (tree, ledger))
    }

    pub(super) fn cleanup_pinned_directory_after_publish(
        token: AttemptToken,
        item: ItemId,
        source: &mut PinnedSourceTree,
        published: &VerifiedPublishedTree,
        ledger: &super::super::journal::JournalDirectoryMove,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<(), CopyError> {
        use super::super::journal::{JournalDirectoryChildIntent, JournalDirectoryRootIntent};
        if cancelled.load(Ordering::Acquire)
            || ledger.item != item.0
            || ledger.cleanup_complete
            || ledger.pending.is_some()
            || ledger.root.is_none()
            || ledger.children.len() != source.entries.len()
            || !verify_pinned_tree_for_cleanup(source, published, ledger)?
        {
            return Err(CopyError::Unsupported);
        }
        let mut order: Vec<usize> = (0..ledger.children.len()).collect();
        order.sort_by(|left, right| {
            ledger.children[*right]
                .relative_components
                .len()
                .cmp(&ledger.children[*left].relative_components.len())
                .then_with(|| {
                    ledger.children[*left]
                        .ordinal
                        .cmp(&ledger.children[*right].ordinal)
                })
        });
        for index in order {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            let child = &ledger.children[index];
            let source_entry = &source.entries[index];
            if child.ordinal as usize != index
                || child.disposition_recorded
                || child.relative_components != source_entry.relative_components
                || child.node_kind != source_entry.node_kind
                || child.source_identity != source_entry.source_identity
                || child.content_fact != source_entry.content_fact
            {
                return Err(CopyError::Unsupported);
            }
            let intent = JournalDirectoryChildIntent {
                ordinal: child.ordinal,
                node_kind: child.node_kind,
                relative_components: child.relative_components.clone(),
                source_identity: child.source_identity.clone(),
                destination_identity: child.destination_identity.clone(),
                content_fact: child.content_fact.clone(),
            };
            emit(ExecutorFact::DirectoryMoveChildIntent {
                token,
                item,
                intent: intent.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::DirectoryMoveChildIntentJournaledExact {
                    token: ack_token,
                    item: ack_item,
                    intent: ack_intent,
                }) if ack_token == token
                    && ack_item == item
                    && ack_intent == intent
                    && !cancelled.load(Ordering::Acquire) => {}
                _ => return Err(CopyError::Cancelled),
            }
            #[cfg(test)]
            if intent.node_kind == crate::operations::journal::JournalDirectoryNodeKind::File
                && intent.relative_components
                    == [
                        "nested".encode_utf16().collect::<Vec<_>>(),
                        "incoming.bin".encode_utf16().collect::<Vec<_>>(),
                    ]
                && std::env::var("GPUI_FILER_DIRECTORY_MOVE_REPLACE_CRASH_PHASE").as_deref()
                    == Ok("source-child-mapping")
            {
                // テスト専用。ACK 消費後、再検証と disposition の前に source handle を観測する。
                let marker =
                    std::env::var_os("GPUI_FILER_DIRECTORY_MOVE_REPLACE_ACK_CONSUMED_MARKER")
                        .map(PathBuf::from)
                        .ok_or(CopyError::Unsupported)?;
                let release =
                    std::env::var_os("GPUI_FILER_DIRECTORY_MOVE_REPLACE_ACK_CONSUMED_RELEASE")
                        .map(PathBuf::from)
                        .ok_or(CopyError::Unsupported)?;
                std::fs::write(marker, b"child intent ACK consumed").map_err(io)?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !release.exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err(CopyError::Unsupported);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if cancelled.load(Ordering::Acquire)
                || !verify_child_for_cleanup(source, published, child, source_entry, true)?
            {
                return Err(CopyError::Unsupported);
            }
            let source_handle = source
                ._handles
                .get(source_entry.handle_index)
                .and_then(Option::as_ref)
                .ok_or(CopyError::Unsupported)?;
            #[cfg(test)]
            if take_directory_move_child_disposition_failure_for_test(
                token,
                item.0,
                &intent.relative_components,
            ) {
                // Destination/source child identities and content were just verified above;
                // inject only the first source-child disposition boundary for this exact attempt.
                return Err(CopyError::Partial(
                    "destination published; injected exact source-child disposition refusal".into(),
                ));
            }
            set_handle_delete_disposition(source_handle)?;
            emit(ExecutorFact::DirectoryMoveChildDisposition {
                token,
                item,
                intent: intent.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::DirectoryMoveChildReceiptJournaledExact {
                    token: ack_token,
                    item: ack_item,
                    intent: ack_intent,
                }) if ack_token == token
                    && ack_item == item
                    && ack_intent == intent
                    && !cancelled.load(Ordering::Acquire) => {}
                _ => return Err(CopyError::Cancelled),
            }
            // Release the exact handle only after its disposition receipt is durable.
            source._handles[source_entry.handle_index] = None;
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let root = ledger.root.as_ref().ok_or(CopyError::Unsupported)?;
        let root_intent = JournalDirectoryRootIntent {
            source_identity: root.source_identity.clone(),
            destination_identity: root.destination_identity.clone(),
        };
        let root_handle = source
            ._handles
            .get(source.root_handle_index)
            .and_then(Option::as_ref)
            .ok_or(CopyError::Unsupported)?;
        let PinnedSourceHandle::Directory(source_root) = root_handle else {
            return Err(CopyError::Unsupported);
        };
        if directory_identity_text(source_root)? != root.source_identity
            || directory_identity_text(&published.root)? != root.destination_identity
            || !enumerate_directory_handle(source_root, 1, 2)?.is_empty()
        {
            return Err(CopyError::Unsupported);
        }
        emit(ExecutorFact::DirectoryMoveRootIntent {
            token,
            item,
            intent: root_intent.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveRootIntentJournaledExact {
                token: ack_token,
                item: ack_item,
                intent: ack_intent,
            }) if ack_token == token
                && ack_item == item
                && ack_intent == root_intent
                && !cancelled.load(Ordering::Acquire) => {}
            _ => return Err(CopyError::Cancelled),
        }
        if cancelled.load(Ordering::Acquire)
            || directory_identity_text(source_root)? != root.source_identity
        {
            return Err(CopyError::Unsupported);
        }
        set_handle_delete_disposition(root_handle)?;
        emit(ExecutorFact::DirectoryMoveRootDisposition {
            token,
            item,
            intent: root_intent.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveRootReceiptJournaledExact {
                token: ack_token,
                item: ack_item,
                intent: ack_intent,
            }) if ack_token == token
                && ack_item == item
                && ack_intent == root_intent
                && !cancelled.load(Ordering::Acquire) => {}
            _ => return Err(CopyError::Cancelled),
        }
        source._handles[source.root_handle_index] = None;
        emit(ExecutorFact::DirectoryMoveComplete { token, item });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveCompleteJournaledExact {
                token: ack_token,
                item: ack_item,
            }) if ack_token == token && ack_item == item && !cancelled.load(Ordering::Acquire) => {
                Ok(())
            }
            _ => Err(CopyError::Cancelled),
        }
    }

    pub(super) fn directory_identity_text(directory: &DirectoryLock) -> Result<String, CopyError> {
        let identity = directory_identity(directory)?;
        Ok(format!(
            "{}:{:08x}{:08x}",
            identity.volume, identity.index_high, identity.index_low
        ))
    }

    #[cfg(test)]
    pub(super) fn directory_identity_for_test(
        path: &Path,
    ) -> Result<crate::filesystem_identity::FilesystemIdentity, CopyError> {
        let route = lock_virtual_destination(path, None)?;
        let directory = route.last().ok_or(CopyError::Unsupported)?;
        let identity = directory_identity(directory)?;
        Ok(crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            identity.volume,
            (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
        ))
    }

    pub(super) fn create_file_undo_candidate(
        parent_path: &Path,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
        name: &std::ffi::OsStr,
    ) -> Result<Option<crate::operations::model::CreatedObjectUndoReceipt>, CopyError> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;

        let route = lock_virtual_destination(parent_path, Some(expected_parent))?;
        let parent = route.last().ok_or(CopyError::Unsupported)?;
        let parent_identity = directory_identity_text(parent)?;
        let component = name.encode_wide().collect::<Vec<_>>();
        let raw_handle = nt_open_or_create_child(
            parent,
            &component,
            false,
            true,
            NT_FILE_CREATE,
            FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
            true,
        )?;
        let file = unsafe { std::fs::File::from_raw_handle(raw_handle.0) };
        let destination = parent_path.join(name);
        let captured = (|| {
            let identity = regular_file_identity(&file)?;
            let signature = leaf_signature(&file)?;
            let content_sha256 = hash_held_file(&file)?;
            if file.metadata().map_err(io)?.len() != 0 || signature.number_of_links != 1 {
                return Err(CopyError::Unsupported);
            }
            Ok(crate::operations::model::CreatedObjectUndoReceipt {
                destination,
                parent_identity,
                object_identity: format!("{}:{:08x}{:08x}", identity.volume, identity.index_high, identity.index_low),
                kind: crate::operations::model::CreatedObjectUndoKind::EmptyFile {
                    content_sha256,
                    signature,
                },
            })
        })();
        // CREATE_FILE succeeded atomically. If proof capture fails, retain the
        // created fact but deliberately emit no undo capability.
        Ok(captured.ok())
    }

    pub(super) fn create_folder_undo_candidate(
        parent_path: &Path,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
        name: &std::ffi::OsStr,
    ) -> Result<Option<crate::operations::model::CreatedObjectUndoReceipt>, CopyError> {
        use std::os::windows::ffi::OsStrExt;

        let route = lock_virtual_destination(parent_path, Some(expected_parent))?;
        let parent = route.last().ok_or(CopyError::Unsupported)?;
        let parent_identity = directory_identity_text(parent)?;
        let component = name.encode_wide().collect::<Vec<_>>();
        let raw_handle = nt_open_or_create_child(
            parent,
            &component,
            true,
            true,
            NT_FILE_CREATE,
            FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
            true,
        )?;
        let directory = DirectoryLock(raw_handle);
        let destination = parent_path.join(name);
        let captured = (|| {
            let identity = directory_identity(&directory)?;
            if !enumerate_directory_handle(&directory, 1, 2)?.is_empty() {
                return Err(CopyError::Unsupported);
            }
            Ok(crate::operations::model::CreatedObjectUndoReceipt {
                destination,
                parent_identity,
                object_identity: format!("{}:{:08x}{:08x}", identity.volume, identity.index_high, identity.index_low),
                kind: crate::operations::model::CreatedObjectUndoKind::EmptyFolder,
            })
        })();
        // The folder exists even when identity/emptiness evidence is unavailable.
        Ok(captured.ok())
    }

    fn directory_identity_text_handle(handle: HANDLE) -> Result<String, CopyError> {
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(handle, &mut info).map_err(io)?;
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(CopyError::Unsupported);
        }
        Ok(format!(
            "{}:{:08x}{:08x}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        ))
    }

    pub(super) fn set_handle_delete_disposition(
        handle: &PinnedSourceHandle,
    ) -> Result<(), CopyError> {
        let raw_handle = match handle {
            PinnedSourceHandle::Directory(directory) => directory.0,
            PinnedSourceHandle::File(file) => raw(file),
        };
        set_raw_handle_delete_disposition(raw_handle)
    }

    pub(super) fn set_raw_handle_delete_disposition(
        raw_handle: ::windows::Win32::Foundation::HANDLE,
    ) -> Result<(), CopyError> {
        let info = FILE_DISPOSITION_INFO {
            DeleteFile: BOOLEAN(1),
        };
        unsafe {
            SetFileInformationByHandle(
                raw_handle,
                FileDispositionInfo,
                &info as *const _ as *const c_void,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .map_err(io)
        }
    }

    fn staging_cleanup_partial(path: &Path, original: CopyError, cleanup: CopyError) -> CopyError {
        CopyError::PrePublishPartial(format!(
            "prepublish staging cleanup failed at {} (source retained); original={original:?}; cleanup={cleanup:?}",
            path.display()
        ))
    }

    pub(super) fn cleanup_staged_handles(
        root: &DirectoryLock,
        expected_root_identity: &str,
        mut nodes: Vec<VerifiedStagedNode>,
    ) -> Result<(), CopyError> {
        if directory_identity_text(root)? != expected_root_identity {
            return Err(CopyError::Unsupported);
        }
        nodes.sort_by_key(|node| std::cmp::Reverse(node.relative_components.len()));
        for node in nodes {
            let identity = match &node.handle {
                PinnedSourceHandle::Directory(directory) => directory_identity_text(directory)?,
                PinnedSourceHandle::File(file) => {
                    let identity = regular_file_identity(file)?;
                    format!(
                        "{}:{:08x}{:08x}",
                        identity.volume, identity.index_high, identity.index_low
                    )
                }
            };
            if identity != node.identity {
                return Err(CopyError::Unsupported);
            }
            if let PinnedSourceHandle::Directory(directory) = &node.handle {
                if !enumerate_directory_handle(directory, 1, 2)?.is_empty() {
                    return Err(CopyError::Unsupported);
                }
            }
            set_handle_delete_disposition(&node.handle)?;
            drop(node);
        }
        if !enumerate_directory_handle(root, 1, 2)?.is_empty() {
            return Err(CopyError::Unsupported);
        }
        set_raw_handle_delete_disposition(root.0)?;
        Ok(())
    }

    fn same_directory_move_replace_backup_manifest(
        entries: &[PinnedSourceEntry],
        children: &[super::super::journal::JournalDirectoryMoveReplaceChild],
    ) -> bool {
        entries.len() == children.len()
            && entries.iter().all(|entry| {
                children.iter().any(|child| {
                    child.relative_components == entry.relative_components
                        && child.node_kind == entry.node_kind
                        && child.identity == entry.source_identity
                        && child.content_fact == entry.content_fact
                })
            })
    }

    fn cleanup_directory_move_replace_backup(
        token: AttemptToken,
        item: ItemId,
        backup_path: &Path,
        destination_parent_handle: HANDLE,
        destination_parent_identity: &str,
        published: &VerifiedPublishedTree,
        replace: &super::super::journal::JournalDirectoryMoveReplace,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<(), CopyError> {
        use super::super::journal::{
            JournalDirectoryMoveReplaceChildIntent as Intent, JournalDirectoryNodeKind,
        };
        let destination_parent_path = published.root_path.parent().ok_or(CopyError::Unsupported)?;
        if backup_path.parent() != Some(destination_parent_path) {
            return Err(CopyError::Partial(
                "displaced backup is outside the published destination parent".into(),
            ));
        }
        if directory_identity_text_handle(destination_parent_handle)? != destination_parent_identity
            || published
                .root_chain
                .last()
                .map(directory_identity_text)
                .transpose()?
                .as_deref()
                != Some(destination_parent_identity)
        {
            return Err(CopyError::Partial(
                "destination parent route changed before backup preflight".into(),
            ));
        }
        let mut backup =
            preflight_pinned_backup_tree(backup_path, cancelled, destination_parent_identity)?;
        if backup.root_identity != replace.backup_root_identity
            || !same_directory_move_replace_backup_manifest(
                &backup.entries,
                &replace.backup_children,
            )
        {
            return Err(CopyError::Partial(
                "displaced backup differs from its approved manifest".into(),
            ));
        }
        let parent_identity = directory_identity_text_handle(destination_parent_handle)?;
        if parent_identity != destination_parent_identity
            || published
                .root_chain
                .last()
                .map(directory_identity_text)
                .transpose()?
                .as_deref()
                != Some(destination_parent_identity)
        {
            return Err(CopyError::Partial(
                "destination parent route changed before backup cleanup".into(),
            ));
        }
        for child in &replace.backup_children {
            if cancelled.load(Ordering::Acquire)
                || directory_identity_text(&published.root).ok().as_deref()
                    != Some(&published.root_identity)
                || directory_identity_text_handle(destination_parent_handle)
                    .ok()
                    .as_deref()
                    != Some(destination_parent_identity)
                || published
                    .root_chain
                    .last()
                    .and_then(|parent| directory_identity_text(parent).ok())
                    .as_deref()
                    != Some(destination_parent_identity)
                || directory_identity_text_handle(
                    match backup
                        ._handles
                        .get(backup.root_handle_index)
                        .and_then(Option::as_ref)
                    {
                        Some(PinnedSourceHandle::Directory(root)) => root.0,
                        _ => return Err(CopyError::Unsupported),
                    },
                )
                .ok()
                .as_deref()
                    != Some(&replace.backup_root_identity)
            {
                return Err(CopyError::Partial(
                    "route or tree identity changed during backup cleanup".into(),
                ));
            }
            let entry = backup
                .entries
                .iter()
                .find(|entry| entry.relative_components == child.relative_components)
                .ok_or(CopyError::Unsupported)?;
            if entry.node_kind != child.node_kind
                || entry.source_identity != child.identity
                || entry.content_fact != child.content_fact
            {
                return Err(CopyError::Partial(
                    "displaced child changed after backup preflight".into(),
                ));
            }
            let intent = Intent {
                ordinal: child.ordinal,
                node_kind: child.node_kind,
                relative_components: child.relative_components.clone(),
                identity: child.identity.clone(),
                content_fact: child.content_fact.clone(),
            };
            emit(ExecutorFact::DirectoryMoveReplaceBackupIntent {
                token,
                item,
                intent: intent.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::DirectoryMoveReplaceBackupIntentJournaledExact {
                    token: ack_token,
                    item: ack_item,
                    intent: ack_intent,
                }) if ack_token == token
                    && ack_item == item
                    && ack_intent == intent
                    && !cancelled.load(Ordering::Acquire) => {}
                _ => {
                    return Err(CopyError::Partial(
                        "backup child intent was not durably acknowledged".into(),
                    ));
                }
            }
            let handle = backup
                ._handles
                .get(entry.handle_index)
                .and_then(Option::as_ref)
                .ok_or(CopyError::Unsupported)?;
            set_handle_delete_disposition(handle)?;
            emit(ExecutorFact::DirectoryMoveReplaceBackupDisposition {
                token,
                item,
                intent: intent.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::DirectoryMoveReplaceBackupReceiptJournaledExact {
                    token: ack_token,
                    item: ack_item,
                    intent: ack_intent,
                }) if ack_token == token && ack_item == item && ack_intent == intent => {
                    backup._handles[entry.handle_index] = None;
                }
                _ => {
                    return Err(CopyError::Partial(
                        "backup child receipt was not durably acknowledged".into(),
                    ));
                }
            }
        }
        let root_intent = Intent {
            ordinal: u32::try_from(replace.backup_children.len())
                .map_err(|_| CopyError::Unsupported)?,
            node_kind: JournalDirectoryNodeKind::Directory,
            relative_components: Vec::new(),
            identity: replace.backup_root_identity.clone(),
            content_fact: "root".into(),
        };
        emit(ExecutorFact::DirectoryMoveReplaceBackupIntent {
            token,
            item,
            intent: root_intent.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplaceBackupIntentJournaledExact {
                token: ack_token,
                item: ack_item,
                intent,
            }) if ack_token == token
                && ack_item == item
                && intent == root_intent
                && !cancelled.load(Ordering::Acquire) => {}
            _ => {
                return Err(CopyError::Partial(
                    "backup root intent was not durably acknowledged".into(),
                ));
            }
        }
        let Some(PinnedSourceHandle::Directory(root)) = backup
            ._handles
            .get(backup.root_handle_index)
            .and_then(Option::as_ref)
        else {
            return Err(CopyError::Unsupported);
        };
        if directory_identity_text(root).ok().as_deref() != Some(&replace.backup_root_identity)
            || directory_identity_text_handle(destination_parent_handle)
                .ok()
                .as_deref()
                != Some(destination_parent_identity)
            || published
                .root_chain
                .last()
                .and_then(|parent| directory_identity_text(parent).ok())
                .as_deref()
                != Some(destination_parent_identity)
            || !enumerate_directory_handle(root, 1, 2)?.is_empty()
            || cancelled.load(Ordering::Acquire)
        {
            return Err(CopyError::Partial(
                "backup root is not the approved empty directory".into(),
            ));
        }
        set_raw_handle_delete_disposition(root.0)?;
        emit(ExecutorFact::DirectoryMoveReplaceBackupDisposition {
            token,
            item,
            intent: root_intent.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplaceBackupReceiptJournaledExact {
                token: ack_token,
                item: ack_item,
                intent,
            }) if ack_token == token && ack_item == item && intent == root_intent => {
                backup._handles[backup.root_handle_index] = None;
            }
            _ => {
                return Err(CopyError::Partial(
                    "backup root receipt was not durably acknowledged".into(),
                ));
            }
        }
        emit(ExecutorFact::DirectoryMoveReplaceBackupComplete { token, item });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplaceBackupCompleteJournaledExact {
                token: ack_token,
                item: ack_item,
            }) if ack_token == token && ack_item == item => {}
            _ => {
                return Err(CopyError::Partial(
                    "backup cleanup completion was not durably acknowledged".into(),
                ));
            }
        }
        match std::fs::symlink_metadata(backup_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CopyError::Partial(format!(
                "backup deletion state is uncertain: {error:?}"
            ))),
            Ok(_) => Err(CopyError::Partial(
                "backup root remains after acknowledged cleanup".into(),
            )),
        }
    }

    fn cleanup_unpublished_staged_tree(staged: VerifiedStagedTree) -> Result<(), CopyError> {
        let path = staged.root_path.clone();
        cleanup_staged_handles(&staged.root, &staged.root_identity, staged.handles)?;
        drop(staged.root);
        drop(staged.root_chain);
        if path.exists() {
            return Err(CopyError::Io(format!(
                "staging root remains after handle disposition: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn reopen_staged_children_by_facts(
        root: &DirectoryLock,
        facts: &[StagedChildFact],
    ) -> Result<Vec<VerifiedStagedNode>, CopyError> {
        use sha2::Digest;
        use std::io::{Read, Seek, SeekFrom};

        fn verify_directory_listing(
            directory: &DirectoryLock,
            parent: &[Vec<u16>],
            facts: &[StagedChildFact],
        ) -> Result<(), CopyError> {
            let mut expected = facts
                .iter()
                .filter(|fact| {
                    fact.relative_components.len() == parent.len() + 1
                        && fact.relative_components[..parent.len()] == *parent
                })
                .map(|fact| {
                    (
                        fact.relative_components.last().unwrap().clone(),
                        fact.node_kind
                            == crate::operations::journal::JournalDirectoryNodeKind::Directory,
                    )
                })
                .collect::<Vec<_>>();
            let mut actual = enumerate_directory_handle(
                directory,
                SOURCE_TREE_MAX_ENTRIES,
                SOURCE_TREE_MAX_NAME_BYTES,
            )?;
            expected.sort();
            actual.sort();
            if actual != expected {
                return Err(CopyError::Unsupported);
            }
            Ok(())
        }

        let mut ordered: Vec<_> = facts.iter().collect();
        ordered.sort_by_key(|fact| fact.relative_components.len());
        verify_directory_listing(root, &[], facts)?;
        let mut opened = Vec::<VerifiedStagedNode>::with_capacity(facts.len());
        let mut buffer = [0u8; 64 * 1024];
        for fact in ordered {
            let parent_components = &fact.relative_components[..fact.relative_components.len() - 1];
            let parent = if parent_components.is_empty() {
                root
            } else {
                opened
                    .iter()
                    .find(|node| {
                        node.relative_components == parent_components
                            && node.node_kind
                                == crate::operations::journal::JournalDirectoryNodeKind::Directory
                    })
                    .and_then(|node| match &node.handle {
                        PinnedSourceHandle::Directory(directory) => Some(directory),
                        PinnedSourceHandle::File(_) => None,
                    })
                    .ok_or(CopyError::Unsupported)?
            };
            verify_directory_listing(parent, parent_components, facts)?;
            let component = fact
                .relative_components
                .last()
                .ok_or(CopyError::Unsupported)?;
            let is_directory =
                fact.node_kind == crate::operations::journal::JournalDirectoryNodeKind::Directory;
            let raw_handle = nt_open_or_create_child(
                parent,
                component,
                is_directory,
                true,
                NT_FILE_OPEN,
                FILE_SHARE_READ.0,
                false,
            )?;
            let handle = if is_directory {
                PinnedSourceHandle::Directory(DirectoryLock(raw_handle))
            } else {
                use std::os::windows::io::FromRawHandle;
                PinnedSourceHandle::File(unsafe { std::fs::File::from_raw_handle(raw_handle.0) })
            };
            let identity = match &handle {
                PinnedSourceHandle::Directory(directory) => directory_identity_text(directory)?,
                PinnedSourceHandle::File(file) => {
                    let identity = regular_file_identity(file)?;
                    let identity_text = format!(
                        "{}:{:08x}{:08x}",
                        identity.volume, identity.index_high, identity.index_low
                    );
                    let mut file = file.try_clone().map_err(io)?;
                    file.seek(SeekFrom::Start(0)).map_err(io)?;
                    let mut hasher = sha2::Sha256::new();
                    loop {
                        let read = file.read(&mut buffer).map_err(io)?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    let content = format!("sha256:{:x}", hasher.finalize());
                    if content != fact.content_fact {
                        return Err(CopyError::Unsupported);
                    }
                    identity_text
                }
            };
            if identity != fact.identity || (is_directory && fact.content_fact != "directory") {
                return Err(CopyError::Unsupported);
            }
            opened.push(VerifiedStagedNode {
                relative_components: fact.relative_components.clone(),
                node_kind: fact.node_kind,
                identity,
                content_fact: fact.content_fact.clone(),
                handle,
            });
        }
        Ok(opened)
    }

    fn cleanup_unpublished_staged_tree_from_facts(
        staged: VerifiedStagedTree,
    ) -> Result<(), CopyError> {
        let path = staged.root_path.clone();
        let handles = reopen_staged_children_by_facts(&staged.root, &staged.nodes)?;
        cleanup_staged_handles(&staged.root, &staged.root_identity, handles)?;
        drop(staged.root);
        drop(staged.root_chain);
        if path.exists() {
            return Err(CopyError::Io(format!(
                "staging root remains after identity-checked cleanup: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn clean_staged_failure(staged: VerifiedStagedTree, original: CopyError) -> PublishTreeFailure {
        let path = staged.root_path.clone();
        match cleanup_unpublished_staged_tree(staged) {
            Ok(()) => PublishTreeFailure::BeforePublish(original),
            Err(cleanup_error) => PublishTreeFailure::BeforePublish(staging_cleanup_partial(
                &path,
                original,
                cleanup_error,
            )),
        }
    }

    fn clean_staged_failure_error(staged: VerifiedStagedTree, original: CopyError) -> CopyError {
        match clean_staged_failure(staged, original) {
            PublishTreeFailure::BeforePublish(error) => error,
            PublishTreeFailure::AfterPublish { error, .. } => error,
        }
    }

    fn clean_staged_failure_from_facts(
        staged: VerifiedStagedTree,
        original: CopyError,
    ) -> PublishTreeFailure {
        let path = staged.root_path.clone();
        match cleanup_unpublished_staged_tree_from_facts(staged) {
            Ok(()) => PublishTreeFailure::BeforePublish(original),
            Err(cleanup_error) => PublishTreeFailure::BeforePublish(staging_cleanup_partial(
                &path,
                original,
                cleanup_error,
            )),
        }
    }

    fn verify_pinned_tree_for_cleanup(
        source: &PinnedSourceTree,
        published: &VerifiedPublishedTree,
        ledger: &super::super::journal::JournalDirectoryMove,
    ) -> Result<bool, CopyError> {
        use std::collections::BTreeSet;
        let source_root = source
            ._handles
            .get(source.root_handle_index)
            .and_then(Option::as_ref)
            .ok_or(CopyError::Unsupported)?;
        let PinnedSourceHandle::Directory(source_root) = source_root else {
            return Err(CopyError::Unsupported);
        };
        let root = ledger.root.as_ref().ok_or(CopyError::Unsupported)?;
        if directory_identity_text(source_root)? != root.source_identity
            || directory_identity_text(&published.root)? != root.destination_identity
        {
            return Ok(false);
        }
        let mut directories = vec![(Vec::<Vec<u16>>::new(), source.root_handle_index)];
        directories.extend(source.entries.iter().filter_map(|entry| {
            (entry.node_kind == super::super::journal::JournalDirectoryNodeKind::Directory)
                .then_some((entry.relative_components.clone(), entry.handle_index))
        }));
        let mut total_entries = 0_usize;
        let mut total_name_bytes = 0_usize;
        for (relative, handle_index) in directories {
            let Some(PinnedSourceHandle::Directory(directory)) =
                source._handles.get(handle_index).and_then(Option::as_ref)
            else {
                return Ok(false);
            };
            let expected = source
                .entries
                .iter()
                .filter_map(|entry| {
                    (entry.relative_components.len() == relative.len() + 1
                        && entry.relative_components[..relative.len()] == relative)
                        .then(|| {
                            (
                                entry.relative_components[relative.len()].clone(),
                                entry.node_kind
                                    == super::super::journal::JournalDirectoryNodeKind::Directory,
                            )
                        })
                })
                .collect::<BTreeSet<_>>();
            let actual_entries = enumerate_directory_handle(
                directory,
                1_024_usize.saturating_sub(total_entries),
                (2 * 1024 * 1024_usize).saturating_sub(total_name_bytes),
            )?;
            total_entries = total_entries.saturating_add(actual_entries.len());
            total_name_bytes = total_name_bytes.saturating_add(
                actual_entries
                    .iter()
                    .map(|(name, _)| name.len().saturating_mul(2))
                    .sum::<usize>(),
            );
            if total_entries > 1_024 || total_name_bytes > 2 * 1024 * 1024 {
                return Ok(false);
            }
            let actual = actual_entries.into_iter().collect::<BTreeSet<_>>();
            if actual != expected {
                return Ok(false);
            }
        }
        for (child, source_entry) in ledger.children.iter().zip(&source.entries) {
            if !verify_child_for_cleanup(source, published, child, source_entry, false)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn verify_child_for_cleanup(
        source: &PinnedSourceTree,
        published: &VerifiedPublishedTree,
        child: &super::super::journal::JournalDirectoryChild,
        source_entry: &PinnedSourceEntry,
        require_empty_directory: bool,
    ) -> Result<bool, CopyError> {
        use std::io::{Seek, SeekFrom};
        let source_handle = source
            ._handles
            .get(source_entry.handle_index)
            .and_then(Option::as_ref)
            .ok_or(CopyError::Unsupported)?;
        let destination_node = published
            .nodes
            .iter()
            .find(|node| node.relative_components == child.relative_components)
            .ok_or(CopyError::Unsupported)?;
        let identity = |handle: &PinnedSourceHandle| -> Result<String, CopyError> {
            match handle {
                PinnedSourceHandle::Directory(directory) => directory_identity_text(directory),
                PinnedSourceHandle::File(file) => {
                    let id = regular_file_identity(file)?;
                    Ok(format!(
                        "{}:{:08x}{:08x}",
                        id.volume, id.index_high, id.index_low
                    ))
                }
            }
        };
        if identity(source_handle)? != child.source_identity
            || identity(&destination_node.handle)? != child.destination_identity
            || child.source_identity != source_entry.source_identity
            || child.content_fact != source_entry.content_fact
        {
            return Ok(false);
        }
        if require_empty_directory
            && child.node_kind == super::super::journal::JournalDirectoryNodeKind::Directory
        {
            let source_directory = match source_handle {
                PinnedSourceHandle::Directory(directory) => directory,
                PinnedSourceHandle::File(_) => return Ok(false),
            };
            if !enumerate_directory_handle(source_directory, 1, 2)?.is_empty() {
                return Ok(false);
            }
        }
        if child.node_kind == super::super::journal::JournalDirectoryNodeKind::File {
            let source_file = match source_handle {
                PinnedSourceHandle::File(file) => file,
                _ => return Ok(false),
            };
            let destination_file = match &destination_node.handle {
                PinnedSourceHandle::File(file) => file,
                _ => return Ok(false),
            };
            use sha2::Digest;
            let mut buffer = [0u8; 64 * 1024];
            for file in [source_file, destination_file] {
                let mut file = file;
                file.seek(SeekFrom::Start(0)).map_err(io)?;
                let mut hasher = sha2::Sha256::new();
                loop {
                    let read = file.read(&mut buffer).map_err(io)?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                }
                if format!("sha256:{:x}", hasher.finalize()) != child.content_fact {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub(super) struct StagedChildFact {
        pub(super) relative_components: Vec<Vec<u16>>,
        pub(super) node_kind: crate::operations::journal::JournalDirectoryNodeKind,
        pub(super) identity: String,
        pub(super) content_fact: String,
    }

    pub(super) struct VerifiedPublishedTree {
        pub(super) root_path: PathBuf,
        pub(super) root_identity: String,
        pub(super) root_chain: Vec<DirectoryLock>,
        pub(super) root_publish_handle: DirectoryLock,
        pub(super) root: DirectoryLock,
        pub(super) nodes: Vec<VerifiedStagedNode>,
    }

    pub(super) enum PublishTreeFailure {
        BeforePublish(CopyError),
        /// Publication succeeded; keep the exact root handle and every child
        /// handle opened before verification stopped for later reconciliation.
        AfterPublish {
            error: CopyError,
            // 可変長の保持treeをBoxに置き、失敗Resultのstackサイズを抑える。
            tree: Box<PartialPublishedTree>,
        },
    }

    pub(super) enum UndoTreePublishFailure {
        BeforePublish(CopyError),
        AfterPublish { error: CopyError },
        RetryableCollision(VerifiedStagedTree),
    }

    impl std::fmt::Debug for PublishTreeFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::BeforePublish(error) => {
                    formatter.debug_tuple("BeforePublish").field(error).finish()
                }
                Self::AfterPublish { error, tree } => formatter
                    .debug_struct("AfterPublish")
                    .field("error", error)
                    .field("root_path", &tree.root_path)
                    .field("root_identity", &tree.root_identity)
                    .field("retained_nodes", &tree.nodes.len())
                    .finish(),
            }
        }
    }

    pub(super) struct PartialPublishedTree {
        pub(super) root_path: PathBuf,
        pub(super) root_identity: String,
        pub(super) root_chain: Vec<DirectoryLock>,
        pub(super) root_publish_handle: DirectoryLock,
        pub(super) reopened_root: Option<DirectoryLock>,
        pub(super) nodes: Vec<VerifiedStagedNode>,
        _staged_handles: Vec<VerifiedStagedNode>,
        /// The latest counterpart handle remains here until every check passes.
        pub(super) pending_node: Option<UnverifiedPublishedNode>,
    }

    pub(super) struct UnverifiedPublishedNode {
        pub(super) relative_components: Vec<Vec<u16>>,
        pub(super) node_kind: crate::operations::journal::JournalDirectoryNodeKind,
        pub(super) identity: String,
        pub(super) content_fact: String,
        pub(super) handle: PinnedSourceHandle,
    }

    pub(super) struct StagingRootLease {
        pub(super) root_path: PathBuf,
        pub(super) root_identity: String,
        pub(super) root_chain: Vec<DirectoryLock>,
        pub(super) root: DirectoryLock,
    }

    pub(super) struct VerifiedStagedNode {
        pub(super) relative_components: Vec<Vec<u16>>,
        pub(super) node_kind: crate::operations::journal::JournalDirectoryNodeKind,
        pub(super) identity: String,
        pub(super) content_fact: String,
        pub(super) handle: PinnedSourceHandle,
    }

    /// Copy the captured tree into an empty attempt-owned directory. This only
    /// creates staging entries; publication and source disposition belong to
    /// later protocol steps.
    pub(super) fn stage_pinned_source_tree(
        tree: &mut PinnedSourceTree,
        staging_root: StagingRootLease,
        cancelled: &AtomicBool,
    ) -> Result<VerifiedStagedTree, CopyError> {
        stage_pinned_source_tree_with_hooks(
            tree,
            staging_root,
            cancelled,
            |_| {},
            flush_staged_file,
        )
    }

    pub(super) fn stage_pinned_source_tree_with_progress(
        tree: &mut PinnedSourceTree,
        staging_root: StagingRootLease,
        cancelled: &AtomicBool,
        on_bytes: impl FnMut(u64),
    ) -> Result<VerifiedStagedTree, CopyError> {
        stage_pinned_source_tree_with_hooks(
            tree,
            staging_root,
            cancelled,
            on_bytes,
            flush_staged_file,
        )
    }

    #[cfg(test)]
    pub(super) fn stage_pinned_source_tree_with_chunk_hook(
        tree: &mut PinnedSourceTree,
        staging_root: StagingRootLease,
        cancelled: &AtomicBool,
        mut after_chunk: impl FnMut(),
    ) -> Result<VerifiedStagedTree, CopyError> {
        stage_pinned_source_tree_with_hooks(
            tree,
            staging_root,
            cancelled,
            |bytes| {
                let _ = bytes;
                after_chunk();
            },
            flush_staged_file,
        )
    }

    fn stage_pinned_source_tree_with_hooks(
        tree: &mut PinnedSourceTree,
        staging_root: StagingRootLease,
        cancelled: &AtomicBool,
        mut on_bytes: impl FnMut(u64),
        mut flush_file: impl FnMut(&std::fs::File) -> Result<(), CopyError>,
    ) -> Result<VerifiedStagedTree, CopyError> {
        use std::io::{Read, Seek, SeekFrom, Write};

        let StagingRootLease {
            root_path,
            root_identity: expected_root_identity,
            root_chain,
            root,
        } = staging_root;
        let root_identity = directory_identity(&root)?;
        let identity_text = format!(
            "{}:{:08x}{:08x}",
            root_identity.volume, root_identity.index_high, root_identity.index_low
        );
        if identity_text != expected_root_identity {
            return Err(staging_cleanup_partial(
                &root_path,
                CopyError::Unsupported,
                CopyError::Unsupported,
            ));
        }

        // Keep created child handles outside the fallible closure so every
        // failure can dispose only entries created and identity-checked here.
        let mut staged_nodes = Vec::with_capacity(tree.entries.len());
        let stage_result = (|| -> Result<Vec<StagedChildFact>, CopyError> {
            if !root_path.is_absolute() {
                return Err(CopyError::Unsupported);
            }
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            // Validate the complete captured shape before any staging mutation.
            if tree.entries.len() > SOURCE_TREE_MAX_ENTRIES {
                return Err(CopyError::Unsupported);
            }
            let mut expected_paths = std::collections::HashSet::new();
            let mut relative_name_bytes = 0usize;
            for entry in &tree.entries {
                if cancelled.load(Ordering::Acquire) {
                    return Err(CopyError::Cancelled);
                }
                if entry.relative_components.is_empty()
                    || entry.relative_components.len() > SOURCE_TREE_MAX_DEPTH
                    || entry.handle_index >= tree._handles.len()
                    || !expected_paths.insert(entry.relative_components.clone())
                {
                    return Err(CopyError::Unsupported);
                }
                for component in &entry.relative_components {
                    if component.is_empty()
                        || component.iter().any(|unit| {
                            *unit == 0
                                || *unit == b'/' as u16
                                || *unit == b'\\' as u16
                                || *unit == b':' as u16
                        })
                        || component == &[b'.' as u16]
                        || component == &[b'.' as u16, b'.' as u16]
                    {
                        return Err(CopyError::Unsupported);
                    }
                    relative_name_bytes = relative_name_bytes
                        .checked_add(component.len().saturating_mul(size_of::<u16>()))
                        .ok_or(CopyError::Unsupported)?;
                }
                if entry.source_identity.is_empty()
                    || relative_name_bytes > SOURCE_TREE_MAX_NAME_BYTES
                {
                    return Err(CopyError::Unsupported);
                }
                match (&entry.node_kind, tree._handles[entry.handle_index].as_ref()) {
                    (
                        crate::operations::journal::JournalDirectoryNodeKind::Directory,
                        Some(PinnedSourceHandle::Directory(_)),
                    )
                    | (
                        crate::operations::journal::JournalDirectoryNodeKind::File,
                        Some(PinnedSourceHandle::File(_)),
                    ) => {}
                    _ => return Err(CopyError::Unsupported),
                }
                if entry.node_kind
                    == crate::operations::journal::JournalDirectoryNodeKind::Directory
                    && entry.content_fact != "directory"
                    || entry.node_kind == crate::operations::journal::JournalDirectoryNodeKind::File
                        && (entry.content_fact.len() != 71
                            || !entry.content_fact.starts_with("sha256:")
                            || !entry.content_fact[7..]
                                .bytes()
                                .all(|byte| byte.is_ascii_hexdigit()))
                {
                    return Err(CopyError::Unsupported);
                }
            }
            for entry in &tree.entries {
                if entry.relative_components.len() > 1 {
                    let parent = &entry.relative_components[..entry.relative_components.len() - 1];
                    if !tree.entries.iter().any(|candidate| {
                        candidate.relative_components == parent
                            && candidate.node_kind
                                == crate::operations::journal::JournalDirectoryNodeKind::Directory
                    }) {
                        return Err(CopyError::Unsupported);
                    }
                }
            }
            let manifest_bytes = serde_json::to_vec(&tree.entries).map_err(io)?.len();
            if manifest_bytes.saturating_mul(2).saturating_add(64 * 1024)
                > SOURCE_TREE_MAX_RECORD_BYTES
            {
                return Err(CopyError::Unsupported);
            }

            let mut ordered: Vec<_> = (0..tree.entries.len()).collect();
            ordered.sort_by_key(|index| tree.entries[*index].relative_components.len());
            use sha2::Digest;
            let mut buffer = [0u8; 64 * 1024];
            for index in ordered {
                if cancelled.load(Ordering::Acquire) {
                    return Err(CopyError::Cancelled);
                }
                let entry = &tree.entries[index];
                let parent_depth = entry.relative_components.len() - 1;
                let parent = if parent_depth == 0 {
                    &root
                } else {
                    staged_nodes
                        .iter()
                        .find(|node: &&VerifiedStagedNode| {
                            node.node_kind
                                == crate::operations::journal::JournalDirectoryNodeKind::Directory
                                && node.relative_components.len() == parent_depth
                                && node.relative_components
                                    == entry.relative_components[..parent_depth]
                        })
                        .map(|node| match &node.handle {
                            PinnedSourceHandle::Directory(directory) => directory,
                            PinnedSourceHandle::File(_) => {
                                unreachable!("directory node checked above")
                            }
                        })
                        .ok_or(CopyError::Unsupported)?
                };
                let component = entry
                    .relative_components
                    .last()
                    .ok_or(CopyError::Unsupported)?;
                match entry.node_kind {
                    crate::operations::journal::JournalDirectoryNodeKind::Directory => {
                        let directory =
                            create_staged_directory(parent, component, true, FILE_SHARE_READ.0)?;
                        let identity = directory_identity_text(&directory)?;
                        staged_nodes.push(VerifiedStagedNode {
                            relative_components: entry.relative_components.clone(),
                            node_kind: entry.node_kind,
                            identity,
                            content_fact: entry.content_fact.clone(),
                            handle: PinnedSourceHandle::Directory(directory),
                        });
                    }
                    crate::operations::journal::JournalDirectoryNodeKind::File => {
                        let Some(PinnedSourceHandle::File(source)) =
                            tree._handles[entry.handle_index].as_mut()
                        else {
                            return Err(CopyError::Unsupported);
                        };
                        source.seek(SeekFrom::Start(0)).map_err(io)?;
                        let target = create_staged_file(parent, component)?;
                        let identity = regular_file_identity(&target)?;
                        let identity = format!(
                            "{}:{:08x}{:08x}",
                            identity.volume, identity.index_high, identity.index_low
                        );
                        staged_nodes.push(VerifiedStagedNode {
                            relative_components: entry.relative_components.clone(),
                            node_kind: entry.node_kind,
                            identity,
                            content_fact: entry.content_fact.clone(),
                            handle: PinnedSourceHandle::File(target),
                        });
                        let Some(PinnedSourceHandle::File(target)) =
                            staged_nodes.last_mut().map(|node| &mut node.handle)
                        else {
                            return Err(CopyError::Unsupported);
                        };
                        let mut hasher = sha2::Sha256::new();
                        loop {
                            if cancelled.load(Ordering::Acquire) {
                                return Err(CopyError::Cancelled);
                            }
                            let read = source.read(&mut buffer).map_err(io)?;
                            if read == 0 {
                                break;
                            }
                            hasher.update(&buffer[..read]);
                            target.write_all(&buffer[..read]).map_err(io)?;
                            on_bytes(read as u64);
                        }
                        if format!("sha256:{:x}", hasher.finalize()) != entry.content_fact {
                            return Err(CopyError::Unsupported);
                        }
                        target.flush().map_err(io)?;
                        flush_file(&target)?;
                        target.seek(SeekFrom::Start(0)).map_err(io)?;
                        let mut verify_hasher = sha2::Sha256::new();
                        loop {
                            if cancelled.load(Ordering::Acquire) {
                                return Err(CopyError::Cancelled);
                            }
                            let read = target.read(&mut buffer).map_err(io)?;
                            if read == 0 {
                                break;
                            }
                            verify_hasher.update(&buffer[..read]);
                        }
                        if format!("sha256:{:x}", verify_hasher.finalize()) != entry.content_fact {
                            return Err(CopyError::Unsupported);
                        }
                        regular_file_identity(&target)?;
                    }
                    _ => return Err(CopyError::Unsupported),
                };
            }
            // Recheck every identity from the retained handles before returning.
            if directory_identity(&root)? != root_identity {
                return Err(CopyError::Unsupported);
            }
            for node in &staged_nodes {
                let identity = match &node.handle {
                    PinnedSourceHandle::Directory(directory) => {
                        let identity = directory_identity(directory)?;
                        format!(
                            "{}:{:08x}{:08x}",
                            identity.volume, identity.index_high, identity.index_low
                        )
                    }
                    PinnedSourceHandle::File(file) => {
                        let identity = regular_file_identity(file)?;
                        format!(
                            "{}:{:08x}{:08x}",
                            identity.volume, identity.index_high, identity.index_low
                        )
                    }
                };
                if identity != node.identity {
                    return Err(CopyError::Unsupported);
                }
            }
            let child_facts = staged_nodes
                .iter()
                .map(|node| StagedChildFact {
                    relative_components: node.relative_components.clone(),
                    node_kind: node.node_kind,
                    identity: node.identity.clone(),
                    content_fact: node.content_fact.clone(),
                })
                .collect();
            Ok(child_facts)
        })();
        let child_facts = match stage_result {
            Ok(facts) => facts,
            Err(error) => {
                return Err(
                    match cleanup_staged_handles(&root, &expected_root_identity, staged_nodes) {
                        Ok(()) => error,
                        Err(cleanup_error) => {
                            staging_cleanup_partial(&root_path, error, cleanup_error)
                        }
                    },
                );
            }
        };
        let checked_identity = match directory_identity(&root) {
            Ok(identity) => format!(
                "{}:{:08x}{:08x}",
                identity.volume, identity.index_high, identity.index_low
            ),
            Err(error) => {
                return Err(staging_cleanup_partial(
                    &root_path,
                    CopyError::Unsupported,
                    error,
                ));
            }
        };
        if checked_identity != expected_root_identity {
            return Err(staging_cleanup_partial(
                &root_path,
                CopyError::Unsupported,
                CopyError::Unsupported,
            ));
        }
        Ok(VerifiedStagedTree {
            root_path,
            root_identity: checked_identity,
            root_chain,
            root,
            nodes: child_facts,
            handles: staged_nodes,
        })
    }

    #[cfg(test)]
    pub(super) fn stage_pinned_source_tree_with_flush_hook(
        tree: &mut PinnedSourceTree,
        staging_root: StagingRootLease,
        cancelled: &AtomicBool,
        flush_file: impl FnMut(&std::fs::File) -> Result<(), CopyError>,
    ) -> Result<VerifiedStagedTree, CopyError> {
        stage_pinned_source_tree_with_hooks(tree, staging_root, cancelled, |_| {}, flush_file)
    }

    pub(super) struct SourceTreeLimits {
        pub(super) entries: usize,
        pub(super) depth: usize,
        pub(super) relative_name_bytes: usize,
        pub(super) record_bytes: usize,
        pub(super) emitted_name_prefix_bytes: usize,
        pub(super) zip_utf8_name_bytes: bool,
    }

    impl Default for SourceTreeLimits {
        fn default() -> Self {
            Self {
                entries: SOURCE_TREE_MAX_ENTRIES,
                depth: SOURCE_TREE_MAX_DEPTH,
                relative_name_bytes: SOURCE_TREE_MAX_NAME_BYTES,
                record_bytes: SOURCE_TREE_MAX_RECORD_BYTES,
                emitted_name_prefix_bytes: 0,
                zip_utf8_name_bytes: false,
            }
        }
    }

    pub(super) fn preflight_pinned_source_tree(
        root: &Path,
        cancelled: &AtomicBool,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits(root, cancelled, SourceTreeLimits::default())
    }

    pub(super) fn preflight_pinned_source_tree_for_directory_replace_rename(
        root: &Path,
        destination_parent: &Path,
        cancelled: &AtomicBool,
    ) -> Result<PinnedSourceTree, CopyError> {
        let destination_parent_pin = open_pinned_directory(destination_parent, false)?;
        let destination_parent_identity = directory_identity_text(&destination_parent_pin)?;
        drop(destination_parent_pin);
        preflight_pinned_source_tree_with_limits_and_shared_parent_inner(
            root,
            cancelled,
            SourceTreeLimits::default(),
            None,
            None,
            true,
            true,
            false,
            None,
            Some((destination_parent, &destination_parent_identity)),
        )
    }

    #[cfg(test)]
    pub(super) fn with_copy_directory_staging_failure_for_test<R>(
        token: AttemptToken,
        item: ItemId,
        run: impl FnOnce() -> R,
    ) -> (R, bool) {
        struct Restore(Option<(AttemptToken, ItemId, std::rc::Rc<std::cell::Cell<bool>>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                COPY_DIRECTORY_STAGING_FAILURE_TEST.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let consumed = std::rc::Rc::new(std::cell::Cell::new(false));
        let previous = COPY_DIRECTORY_STAGING_FAILURE_TEST.with(|slot| {
            slot.borrow_mut().replace((token, item, consumed.clone()))
        });
        let _restore = Restore(previous);
        let result = run();
        (result, consumed.get())
    }

    #[cfg(test)]
    pub(super) fn take_copy_directory_staging_failure_for_test(
        token: AttemptToken,
        item: ItemId,
    ) -> bool {
        COPY_DIRECTORY_STAGING_FAILURE_TEST.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|(armed_token, armed_item, _)| *armed_token == token && *armed_item == item) {
                let (_, _, consumed) = slot.take().expect("matching scoped staging fault");
                assert!(!consumed.replace(true), "targeted staging fault is one-shot");
                true
            } else {
                false
            }
        })
    }

    #[cfg(test)]
    pub(super) fn with_copy_directory_stage_write_lock_for_test<R>(
        token: AttemptToken,
        item: ItemId,
        hook: impl FnOnce(&std::fs::File) + Send + 'static,
        run: impl FnOnce() -> R,
    ) -> (R, bool) {
        struct Restore(Option<(AttemptToken, ItemId, std::rc::Rc<std::cell::Cell<bool>>, Box<dyn FnOnce(&std::fs::File)>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                COPY_DIRECTORY_STAGE_WRITE_LOCK_TEST.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let consumed = std::rc::Rc::new(std::cell::Cell::new(false));
        let previous = COPY_DIRECTORY_STAGE_WRITE_LOCK_TEST.with(|slot| {
            slot.borrow_mut().replace((token, item, consumed.clone(), Box::new(hook)))
        });
        let _restore = Restore(previous);
        let result = run();
        (result, consumed.get())
    }

    #[cfg(test)]
    fn run_copy_directory_stage_write_lock_hook_for_test(token: AttemptToken, item: ItemId, file: &std::fs::File) {
        COPY_DIRECTORY_STAGE_WRITE_LOCK_TEST.with(|slot| {
            let hook = {
                let mut slot = slot.borrow_mut();
                if slot.as_ref().is_some_and(|(armed_token, armed_item, _, _)| *armed_token == token && *armed_item == item) {
                    let (_, _, consumed, hook) = slot.take().expect("matching stage write-lock hook");
                    assert!(!consumed.replace(true), "targeted stage write-lock hook is one-shot");
                    Some(hook)
                } else {
                    None
                }
            };
            if let Some(hook) = hook { hook(file); }
        });
    }

    /// Recovery Copy pins regular files and directories with the same typed
    /// no-follow identity captured at admission. These handles deliberately do
    /// not request DELETE, so the normal Copy source adapter can reopen them.
    pub(super) fn preflight_recovery_copy_source(
        root: &Path,
        expected: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &AtomicBool,
    ) -> Result<PinnedSourceTree, CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let metadata = std::fs::symlink_metadata(root).map_err(io)?;
        if metadata.file_type().is_symlink() {
            return Err(CopyError::Unsupported);
        }
        if metadata.file_type().is_dir() {
            let pinned = preflight_pinned_source_tree_with_limits_and_shared_parent(
                root,
                cancelled,
                SourceTreeLimits::default(),
                None,
                None,
                false,
                false,
                false,
                None,
            )?;
            let Some(PinnedSourceHandle::Directory(directory)) = pinned
                ._handles
                .get(pinned.root_handle_index)
                .and_then(Option::as_ref)
            else {
                return Err(CopyError::Unsupported);
            };
            let identity = directory_identity(directory)?;
            let observed = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                identity.volume,
                (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
            );
            if &observed != expected {
                return Err(CopyError::GuardRejected(
                    "recovery Copy directory identity changed during preflight".into(),
                ));
            }
            return Ok(pinned);
        }
        if !metadata.file_type().is_file() {
            return Err(CopyError::Unsupported);
        }
        let parent = root.parent().ok_or(CopyError::Unsupported)?;
        let mut handles = lock_non_reparse_ancestors(parent)?
            .into_iter()
            .map(|directory| Some(PinnedSourceHandle::Directory(directory)))
            .collect::<Vec<_>>();
        let file = open_pinned_file_with_access(root, false)?;
        let identity = regular_file_identity(&file)?;
        if &identity.shared() != expected {
            return Err(CopyError::GuardRejected(
                "recovery Copy file identity changed during preflight".into(),
            ));
        }
        let (volume, index) = expected.windows_parts().ok_or(CopyError::Unsupported)?;
        let root_identity = format!("{volume:08x}:{index:016x}");
        handles.push(Some(PinnedSourceHandle::File(file)));
        let root_handle_index = handles.len() - 1;
        Ok(PinnedSourceTree {
            entries: Vec::new(),
            root_identity,
            root_handle_index,
            _handles: handles,
        })
    }

    /// ZIP writes its stage beside the pinned source root, so the source parent
    /// must permit the operation's own stage creation while remaining identity-pinned.
    pub(super) fn preflight_pinned_source_tree_with_shared_parent(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
        expected_root_identity: &str,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_shared_parent_policy(
            root,
            cancelled,
            expected_parent_identity,
            expected_root_identity,
            false,
        )
    }

    pub(super) fn preflight_pinned_source_tree_for_zip_with_shared_parent(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
        expected_root_identity: &str,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_shared_parent_policy(
            root,
            cancelled,
            expected_parent_identity,
            expected_root_identity,
            true,
        )
    }

    pub(super) fn preflight_pinned_source_tree_for_zip_with_limits(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
        expected_root_identity: &str,
        limits: SourceTreeLimits,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits_and_shared_parent(
            root,
            cancelled,
            limits,
            Some(expected_parent_identity),
            Some(expected_root_identity),
            false,
            false,
            true,
            None,
        )
    }

    /// 7z の capture では NTFS identity と no-follow tree pins を必須にする。
    pub(super) fn preflight_pinned_source_tree_for_7z(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
        expected_root_identity: &str,
        limits: SourceTreeLimits,
        deadline: Instant,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits_and_shared_parent(
            root,
            cancelled,
            limits,
            Some(expected_parent_identity),
            Some(expected_root_identity),
            false,
            false,
            false,
            Some(deadline),
        )
    }

    fn preflight_pinned_source_tree_with_shared_parent_policy(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
        expected_root_identity: &str,
        allow_exfat: bool,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits_and_shared_parent(
            root,
            cancelled,
            SourceTreeLimits::default(),
            Some(expected_parent_identity),
            Some(expected_root_identity),
            false,
            false,
            allow_exfat,
            None,
        )
    }

    pub(super) fn move_cross_volume_directory(
        token: AttemptToken,
        item: ItemId,
        source_path: &Path,
        destination_path: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_destination_parent: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        on_bytes: &mut dyn FnMut(u64),
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<crate::operations::model::MoveUndoReceipt, CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        if destination_path.exists() {
            return Err(CopyError::DestinationExists);
        }
        if !source_path.is_absolute() || !destination_path.is_absolute() {
            return Err(CopyError::Unsupported);
        }
        let destination_parent = destination_path.parent().ok_or(CopyError::Unsupported)?;
        ensure_local_fixed_ntfs(destination_parent)?;
        let source_parent = source_path.parent().ok_or(CopyError::Unsupported)?;
        let mut source_route_paths = Vec::new();
        let source_route = lock_destination_route_root_first(source_parent, |path| {
            source_route_paths.push(path.to_path_buf());
        })?;
        let source_parent_route = parent_route_proof(&source_route_paths, &source_route)?;
        let mut destination_route_paths = Vec::new();
        let destination_route = lock_destination_route_root_first(destination_parent, |path| {
            destination_route_paths.push(path.to_path_buf());
        })?;
        let destination_parent_route =
            parent_route_proof(&destination_route_paths, &destination_route)?;
        let destination_parent_handle = open_pinned_destination_parent(destination_parent)?;
        let destination_identity = directory_identity(&destination_parent_handle)?;
        let Some((expected_destination_volume, expected_destination_index)) =
            expected_destination_parent.windows_parts()
        else {
            return Err(CopyError::Unsupported);
        };
        if destination_identity.volume != expected_destination_volume
            || (u64::from(destination_identity.index_high) << 32
                | u64::from(destination_identity.index_low))
                != expected_destination_index
        {
            return Err(CopyError::GuardRejected(
                "destination directory identity changed".into(),
            ));
        }
        let mut source_tree = preflight_pinned_source_tree(source_path, cancelled)?;
        let source_handle = source_tree
            ._handles
            .get(source_tree.root_handle_index)
            .and_then(Option::as_ref)
            .ok_or(CopyError::Unsupported)?;
        let PinnedSourceHandle::Directory(source_root) = source_handle else {
            return Err(CopyError::Unsupported);
        };
        let source_identity = directory_identity(source_root)?;
        let Some((expected_source_volume, expected_source_index)) = expected_source.windows_parts()
        else {
            return Err(CopyError::Unsupported);
        };
        if source_identity.volume != expected_source_volume
            || (u64::from(source_identity.index_high) << 32 | u64::from(source_identity.index_low))
                != expected_source_index
        {
            return Err(CopyError::GuardRejected(
                "source directory identity changed".into(),
            ));
        }
        if source_identity.volume == destination_identity.volume {
            return Err(CopyError::Unsupported);
        }
        let staging_path = destination_parent.join(format!(
            ".gpui-filer-move-stage-{}-{}-{}",
            token.job_id().value(),
            token.attempt_id().value(),
            item.0,
        ));
        let staging_root = create_attempt_staging_root(&staging_path)?;
        let staged = stage_pinned_source_tree_with_progress(
            &mut source_tree,
            staging_root,
            cancelled,
            on_bytes,
        )?;
        let (published, ledger) = match register_manifest_publish_with_ledger(
            token,
            item,
            source_path,
            destination_path,
            &source_tree,
            staged,
            cancelled,
            controls,
            emit,
        ) {
            Ok(published) => published,
            Err(PublishTreeFailure::BeforePublish(error)) => return Err(error),
            Err(PublishTreeFailure::AfterPublish { error, .. }) => {
                return Err(CopyError::Partial(format!(
                    "destination published but verification failed: {error:?}"
                )));
            }
        };
        cleanup_pinned_directory_after_publish(
            token,
            item,
            &mut source_tree,
            &published,
            &ledger,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "destination published; source cleanup requires recovery: {error:?}"
            ))
        })?;
        let mut completed_manifest = ledger;
        completed_manifest.cleanup_complete = true;
        for child in &mut completed_manifest.children {
            child.disposition_recorded = true;
        }
        let root = completed_manifest.root.as_mut().ok_or(CopyError::Unsupported)?;
        root.disposition_recorded = true;
        let destination_root_identity = directory_identity(&published.root)?;
        Ok(crate::operations::model::MoveUndoReceipt {
            source: source_path.to_path_buf(),
            destination: destination_path.to_path_buf(),
            object_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                destination_root_identity.volume,
                (u64::from(destination_root_identity.index_high) << 32)
                    | u64::from(destination_root_identity.index_low),
            ),
            object_kind: crate::operations::model::MoveUndoObjectKind::Directory,
            source_parent_route,
            destination_parent_route,
            cross_volume_file: Some(crate::operations::model::CrossVolumeMoveUndoProof {
                original_source_identity: expected_source.clone(),
                original_source_signature: crate::operations::model::SourceCleanupLeafSignature {
                    number_of_links: 0,
                    change_time: 0,
                },
                destination_signature: crate::operations::model::SourceCleanupLeafSignature {
                    number_of_links: 0,
                    change_time: 0,
                },
                content_sha256: String::new(),
                directory_manifest: Some(completed_manifest),
            }),
        })
    }

    pub(super) fn replace_move_cross_volume_directory(
        token: AttemptToken,
        item: ItemId,
        source_path: &Path,
        destination_path: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_destination_parent: &crate::filesystem_identity::FilesystemIdentity,
        mut source: PinnedSourceTree,
        mut target: PinnedSourceTree,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        on_bytes: &mut dyn FnMut(u64),
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<(), CopyError> {
        use super::super::journal::DirectoryMoveReplacePhase as Phase;
        if cancelled.load(Ordering::Acquire)
            || item.0 != 0
            || !source_path.is_absolute()
            || !destination_path.is_absolute()
            || source.root_identity == target.root_identity
        {
            return Err(CopyError::Cancelled);
        }
        let destination_parent = destination_path.parent().ok_or(CopyError::Unsupported)?;
        ensure_local_fixed_ntfs(destination_parent)?;
        let Some((expected_parent_volume, expected_parent_index)) =
            expected_destination_parent.windows_parts()
        else {
            return Err(CopyError::Unsupported);
        };
        let target_parent_index = target
            .root_handle_index
            .checked_sub(1)
            .ok_or(CopyError::Unsupported)?;
        let (target_parent_identity, target_parent_matches_expected) = match target
            ._handles
            .get(target_parent_index)
            .and_then(Option::as_ref)
        {
            Some(PinnedSourceHandle::Directory(parent)) => {
                let identity = directory_identity(parent)?;
                let index = (u64::from(identity.index_high) << 32) | u64::from(identity.index_low);
                (
                    directory_identity_text(parent)?,
                    identity.volume == expected_parent_volume && index == expected_parent_index,
                )
            }
            _ => return Err(CopyError::Unsupported),
        };
        if !target_parent_matches_expected {
            return Err(CopyError::GuardRejected(
                "pinned destination parent identity changed before route release".into(),
            ));
        }
        // 旧target route pinはread-only sharingのため、同じ親にstageを作る書込みを妨げる。親identityを先に照合し、このpinだけ解放する。
        target._handles[target_parent_index] = None;
        let destination_parent_handle = open_pinned_destination_parent(destination_parent)?;
        let destination_parent_id = directory_identity(&destination_parent_handle)?;
        let destination_parent_index = (u64::from(destination_parent_id.index_high) << 32)
            | u64::from(destination_parent_id.index_low);
        if destination_parent_id.volume != expected_parent_volume
            || destination_parent_index != expected_parent_index
            || directory_identity_text(&destination_parent_handle)? != target_parent_identity
        {
            return Err(CopyError::GuardRejected(
                "destination parent identity changed before directory Replace".into(),
            ));
        }
        let Some((expected_source_volume, expected_source_index)) = expected_source.windows_parts()
        else {
            return Err(CopyError::Unsupported);
        };
        let source_root = match source
            ._handles
            .get(source.root_handle_index)
            .and_then(Option::as_ref)
        {
            Some(PinnedSourceHandle::Directory(root)) => root,
            _ => return Err(CopyError::Unsupported),
        };
        let source_id = directory_identity(source_root)?;
        let source_index = (u64::from(source_id.index_high) << 32) | u64::from(source_id.index_low);
        if source_id.volume != expected_source_volume || source_index != expected_source_index {
            return Err(CopyError::GuardRejected(
                "source directory identity changed before Replace".into(),
            ));
        }
        if source_id.volume == destination_parent_id.volume {
            return Err(CopyError::Unsupported);
        }

        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| {
            CopyError::Io(format!("generate directory Replace identity: {error}"))
        })?;
        let suffix = format!("{:032x}", u128::from_be_bytes(random));
        let stage_path = destination_parent.join(format!(
            ".gpui-filer-move-replace-stage-{}-{}-{suffix}",
            token.job_id().value(),
            token.attempt_id().value()
        ));
        let backup_path = destination_parent.join(format!(
            ".gpui-filer-move-replace-backup-{}-{}-{suffix}",
            token.job_id().value(),
            token.attempt_id().value()
        ));
        if stage_path.exists() || backup_path.exists() {
            return Err(CopyError::Unsupported);
        }
        let staging_root = create_attempt_staging_root(&stage_path)?;
        let staged = match stage_pinned_source_tree_with_progress(
            &mut source,
            staging_root,
            cancelled,
            on_bytes,
        ) {
            Ok(staged) => staged,
            Err(error) => return Err(error),
        };
        let staged_parent_identity =
            match directory_identity_text_handle(destination_parent_handle.0) {
                Ok(identity) => identity,
                Err(error) => return Err(clean_staged_failure_error(staged, error)),
            };
        if staged_parent_identity != target_parent_identity {
            return Err(clean_staged_failure_error(
                staged,
                CopyError::GuardRejected(
                    "destination parent identity changed during staging".into(),
                ),
            ));
        }
        let ledger = match build_directory_move_replace_ledger(
            item,
            source_path,
            destination_path,
            &stage_path,
            &backup_path,
            &target_parent_identity,
            &source,
            &staged,
            &target,
        ) {
            Ok(ledger) => ledger,
            Err(error) => return Err(clean_staged_failure_error(staged, error)),
        };

        // Persist the exact backup route first; the combined source/backup
        // manifests are then registered before any target mutation.
        emit(ExecutorFact::ReplaceBackupIntent {
            token,
            item,
            source: source_path.to_path_buf(),
            destination: destination_path.to_path_buf(),
            backup: backup_path.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::ReplaceBackupJournaledExact {
                token: ack_token,
                item: ack_item,
                source: ack_source,
                destination: ack_destination,
                backup: ack_backup,
            }) if ack_token == token
                && ack_item == item
                && ack_source == source_path
                && ack_destination == destination_path
                && ack_backup == backup_path
                && !cancelled.load(Ordering::Acquire) => {}
            _ => return Err(clean_staged_failure_error(staged, CopyError::Cancelled)),
        }
        emit(ExecutorFact::DirectoryMoveManifestIntent {
            token,
            ledger: ledger.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveManifestJournaledExact {
                token: ack_token,
                ledger: ack_ledger,
            }) if ack_token == token
                && ack_ledger == ledger
                && !cancelled.load(Ordering::Acquire) => {}
            _ => return Err(clean_staged_failure_error(staged, CopyError::Cancelled)),
        }

        if let Err(intent_error) = journal_directory_move_replace_phase(
            token,
            item,
            Phase::DisplaceIntent,
            cancelled,
            controls,
            emit,
        ) {
            if let Err(cleanup_error) = cleanup_unpublished_staged_tree(staged) {
                return Err(CopyError::Partial(format!(
                    "pre-displacement stage cleanup failed after {intent_error:?}: {cleanup_error:?}; inspect stage {}",
                    stage_path.display()
                )));
            }
            journal_directory_move_replace_phase(
                token,
                item,
                Phase::StageDiscarded,
                cancelled,
                controls,
                emit,
            )
            .map_err(|discard_error| {
                CopyError::Partial(format!(
                    "stage was discarded but its terminal journal phase failed after {intent_error:?}: {discard_error:?}"
                ))
            })?;
            return Err(intent_error);
        }
        if cancelled.load(Ordering::Acquire) {
            cleanup_unpublished_staged_tree(staged)?;
            journal_directory_move_replace_phase(
                token,
                item,
                Phase::StageDiscarded,
                cancelled,
                controls,
                emit,
            )?;
            return Err(CopyError::Cancelled);
        }

        let target_root = match target
            ._handles
            .get(target.root_handle_index)
            .and_then(Option::as_ref)
        {
            Some(PinnedSourceHandle::Directory(root)) => root.0,
            _ => return Err(clean_staged_failure_error(staged, CopyError::Unsupported)),
        };
        if directory_identity_text_handle(target_root)? != target.root_identity
            || directory_identity_text_handle(destination_parent_handle.0)?
                != target_parent_identity
        {
            cleanup_unpublished_staged_tree(staged)?;
            journal_directory_move_replace_phase(
                token,
                item,
                Phase::StageDiscarded,
                cancelled,
                controls,
                emit,
            )?;
            return Err(CopyError::GuardRejected(
                "destination tree changed before displacement".into(),
            ));
        }
        for entry in &target.entries {
            target._handles[entry.handle_index] = None;
        }
        if cancelled.load(Ordering::Acquire) {
            cleanup_unpublished_staged_tree(staged)?;
            journal_directory_move_replace_phase(
                token,
                item,
                Phase::StageDiscarded,
                cancelled,
                controls,
                emit,
            )?;
            return Err(CopyError::Cancelled);
        }
        if let Err(error) = publish_no_replace_handle(target_root, &backup_path) {
            return Err(CopyError::Partial(format!(
                "target displacement result is uncertain: {error:?}; retain staged tree at {} and inspect destination and backup {}",
                stage_path.display(),
                backup_path.display()
            )));
        }
        target._handles[target.root_handle_index] = None;
        journal_directory_move_replace_phase(
            token,
            item,
            Phase::TargetDisplaced,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "target displaced; journal receipt failed: {error:?}"
            ))
        })?;
        journal_directory_move_replace_phase(
            token,
            item,
            Phase::PublishIntent,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::PrePublishPartial(format!(
                "target displaced; publication intent failed: {error:?}; backup retained at {}",
                backup_path.display()
            ))
        })?;
        let published = match publish_and_verify_staged_tree(staged, destination_path, cancelled) {
            Ok(published) => published,
            Err(PublishTreeFailure::BeforePublish(error)) => {
                return Err(CopyError::PrePublishPartial(format!(
                    "target displaced; new directory was not published: {error:?}; backup retained at {}",
                    backup_path.display()
                )));
            }
            Err(PublishTreeFailure::AfterPublish { error, tree }) => {
                return Err(CopyError::Partial(format!(
                    "new directory published at {}; verification failed: {error:?}; backup retained at {}",
                    tree.root_path.display(),
                    backup_path.display()
                )));
            }
        };
        journal_directory_move_replace_phase(
            token,
            item,
            Phase::DestinationPublished,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "destination published; journal receipt failed: {error:?}; backup retained at {}",
                backup_path.display()
            ))
        })?;
        journal_directory_move_replace_phase(
            token,
            item,
            Phase::SourceCleanup,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "destination published; source retained: {error:?}; backup retained at {}",
                backup_path.display()
            ))
        })?;
        cleanup_pinned_directory_after_publish(
            token,
            item,
            &mut source,
            &published,
            &ledger,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| CopyError::Partial(format!(
            "destination published; source cleanup is incomplete: {error:?}; backup retained at {}",
            backup_path.display()
        )))?;
        journal_directory_move_replace_phase(
            token,
            item,
            Phase::BackupCleanup,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "source removed; backup cleanup could not start: {error:?}; backup retained at {}",
                backup_path.display()
            ))
        })?;
        cleanup_directory_move_replace_backup(
            token,
            item,
            &backup_path,
            destination_parent_handle.0,
            &target_parent_identity,
            &published,
            ledger.replace.as_ref().ok_or(CopyError::Unsupported)?,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "source removed; displaced backup cleanup is incomplete: {error:?}; backup at {}",
                backup_path.display()
            ))
        })?;
        journal_directory_move_replace_phase(
            token,
            item,
            Phase::Complete,
            cancelled,
            controls,
            emit,
        )
        .map_err(|error| {
            CopyError::Partial(format!(
                "source and backup cleanup completed but terminal receipt failed: {error:?}"
            ))
        })?;
        Ok(())
    }

    pub(super) fn preflight_pinned_source_tree_with_limits(
        root: &Path,
        cancelled: &AtomicBool,
        limits: SourceTreeLimits,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits_and_shared_parent(
            root, cancelled, limits, None, None, true, true, false, None,
        )
    }

    fn preflight_pinned_backup_tree(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits_and_shared_parent(
            root,
            cancelled,
            SourceTreeLimits::default(),
            Some(expected_parent_identity),
            None,
            true,
            true,
            false,
            None,
        )
    }

    fn preflight_pinned_source_tree_with_limits_and_shared_parent(
        root: &Path,
        cancelled: &AtomicBool,
        limits: SourceTreeLimits,
        shared_parent_identity: Option<&str>,
        expected_root_identity: Option<&str>,
        delete_capable: bool,
        hash_contents: bool,
        allow_exfat: bool,
        deadline: Option<Instant>,
    ) -> Result<PinnedSourceTree, CopyError> {
        preflight_pinned_source_tree_with_limits_and_shared_parent_inner(
            root,
            cancelled,
            limits,
            shared_parent_identity,
            expected_root_identity,
            delete_capable,
            hash_contents,
            allow_exfat,
            deadline,
            None,
        )
    }

    fn preflight_pinned_source_tree_with_limits_and_shared_parent_inner(
        root: &Path,
        cancelled: &AtomicBool,
        limits: SourceTreeLimits,
        shared_parent_identity: Option<&str>,
        expected_root_identity: Option<&str>,
        delete_capable: bool,
        hash_contents: bool,
        allow_exfat: bool,
        deadline: Option<Instant>,
        share_write_route_identity: Option<(&Path, &str)>,
    ) -> Result<PinnedSourceTree, CopyError> {
        use std::path::{Component, Prefix};

        if cancelled.load(Ordering::Acquire)
            || deadline.is_some_and(|limit| Instant::now() >= limit)
        {
            return Err(CopyError::Cancelled);
        }
        let absolute = if root.is_absolute() {
            root.to_path_buf()
        } else {
            return Err(CopyError::Unsupported);
        };
        let mut components = absolute.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(CopyError::Unsupported);
        };
        let Prefix::Disk(_) = prefix.kind() else {
            return Err(CopyError::Unsupported);
        };
        if components.any(|component| matches!(component, Component::ParentDir)) {
            return Err(CopyError::Unsupported);
        }
        if allow_exfat {
            // ZIP 専用の volume 許可を広げても source / parent の保持 handle と identity 照合は省略せず、通常操作の fixed NTFS 制限は別 policy に保つ。
            ensure_local_archive_volume(&absolute)?;
        } else {
            ensure_local_fixed_ntfs(&absolute)?;
        }

        let mut handles = Vec::new();
        let mut ancestor_paths = Vec::new();
        let mut cursor = absolute.parent();
        while let Some(path) = cursor {
            ancestor_paths.push(path.to_path_buf());
            cursor = path.parent();
        }
        ancestor_paths.reverse();
        for path in ancestor_paths {
            if cancelled.load(Ordering::Acquire)
                || deadline.is_some_and(|limit| Instant::now() >= limit)
            {
                return Err(CopyError::Cancelled);
            }
            // Path表記の大小文字ではなく、先に開いたidentity probeで共有範囲を決める。
            let matches_e7_destination_parent = if let Some((_, expected_identity)) = share_write_route_identity {
                let probe = open_pinned_directory(&path, false)?;
                directory_identity_text(&probe)? == expected_identity
            } else {
                false
            };
            let directory = if matches_e7_destination_parent {
                let expected_identity = share_write_route_identity
                    .map(|(_, identity)| identity)
                    .ok_or(CopyError::Unsupported)?;
                let directory = open_pinned_directory_with_share(
                    &path,
                    false,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )?;
                if directory_identity_text(&directory)? != expected_identity {
                    return Err(CopyError::Partial(
                        "destination parent identity changed during E7 route pinning".into(),
                    ));
                }
                directory
            } else if let (Some(expected_identity), Some(parent)) =
                (shared_parent_identity, absolute.parent())
            {
                if path.as_path() != parent {
                    if allow_exfat {
                        // 祖先は経路固定のため保持し、書込み共有を許してもdelete共有を拒否してrenameを防ぐ。
                        open_pinned_directory_with_share(
                            &path,
                            false,
                            FILE_SHARE_READ | FILE_SHARE_WRITE,
                        )?
                    } else {
                        open_pinned_directory(&path, false)?
                    }
                } else {
                    let directory = open_pinned_directory_with_share(
                        &path,
                        false,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                    )?;
                    if directory_identity_text(&directory)? != expected_identity {
                        return Err(CopyError::Partial(
                            "displaced backup parent identity changed during preflight".into(),
                        ));
                    }
                    directory
                }
            } else {
                open_pinned_directory(&path, false)?
            };
            handles.push(Some(PinnedSourceHandle::Directory(directory)));
        }

        let root_handle = open_pinned_directory(&absolute, delete_capable)?;
        let root_identity = directory_identity(&root_handle)?;
        let root_identity = format!(
            "{}:{:08x}{:08x}",
            root_identity.volume, root_identity.index_high, root_identity.index_low
        );
        if expected_root_identity.is_some_and(|expected| expected != root_identity) {
            return Err(CopyError::GuardRejected(
                "source directory identity changed before traversal".into(),
            ));
        }
        handles.push(Some(PinnedSourceHandle::Directory(root_handle)));
        let root_index = handles.len() - 1;
        let mut entries = Vec::new();
        let mut name_bytes = 0usize;
        walk_pinned_source_directory(
            &absolute,
            Vec::new(),
            0,
            root_index,
            &mut handles,
            &mut entries,
            &mut name_bytes,
            cancelled,
            &limits,
            delete_capable,
            hash_contents,
            deadline,
        )?;
        Ok(PinnedSourceTree {
            entries,
            root_identity,
            root_handle_index: root_index,
            _handles: handles,
        })
    }

    pub(super) fn ensure_local_fixed_ntfs(path: &Path) -> Result<(), CopyError> {
        let mut drive = path
            .components()
            .next()
            .ok_or(CopyError::Unsupported)?
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>();
        drive.extend([b'\\' as u16, 0]);
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) };
        if drive_type != 3 {
            return Err(CopyError::Unsupported);
        }
        let mut filesystem = [0u16; 32];
        unsafe {
            GetVolumeInformationW(
                PCWSTR(drive.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
            .map_err(io)?;
        }
        let name = String::from_utf16_lossy(
            &filesystem[..filesystem
                .iter()
                .position(|u| *u == 0)
                .unwrap_or(filesystem.len())],
        );
        if !name.eq_ignore_ascii_case("NTFS") {
            return Err(CopyError::Unsupported);
        }
        Ok(())
    }

    fn ensure_local_archive_volume(path: &Path) -> Result<(), CopyError> {
        let mut drive = path
            .components()
            .next()
            .ok_or(CopyError::Unsupported)?
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>();
        drive.extend([b'\\' as u16, 0]);
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) };
        #[cfg(test)]
        let drive_type = archive_drive_type_override_for_test(path, drive_type);
        let mut filesystem = [0u16; 32];
        unsafe {
            GetVolumeInformationW(
                PCWSTR(drive.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
            .map_err(io)?;
        }
        let name = String::from_utf16_lossy(
            &filesystem[..filesystem
                .iter()
                .position(|u| *u == 0)
                .unwrap_or(filesystem.len())],
        );
        if !archive_source_volume_allowed(drive_type, &name) {
            return Err(CopyError::Unsupported);
        }
        Ok(())
    }

    pub(super) fn ensure_ntfs_archive_volume(path: &Path) -> Result<(), CopyError> {
        ensure_local_archive_volume(path)?;
        let mut drive = path
            .components()
            .next()
            .ok_or(CopyError::Unsupported)?
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>();
        drive.extend([b'\\' as u16, 0]);
        let mut filesystem = [0u16; 32];
        unsafe {
            GetVolumeInformationW(
                PCWSTR(drive.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
            .map_err(io)?;
        }
        let name = String::from_utf16_lossy(
            &filesystem[..filesystem
                .iter()
                .position(|u| *u == 0)
                .unwrap_or(filesystem.len())],
        );
        if !name.eq_ignore_ascii_case("NTFS") {
            return Err(CopyError::Unsupported);
        }
        Ok(())
    }

    fn archive_source_volume_allowed(drive_type: u32, filesystem: &str) -> bool {
        matches!(drive_type, 2 | 3)
            && (filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("exFAT"))
    }

    #[cfg(test)]
    static ARCHIVE_DRIVE_TYPE_OVERRIDE_FOR_TEST: OnceLock<
        std::sync::Mutex<Option<(PathBuf, u32)>>,
    > = OnceLock::new();

    #[cfg(test)]
    fn archive_drive_type_override_for_test(path: &Path, actual: u32) -> u32 {
        let mut configured = ARCHIVE_DRIVE_TYPE_OVERRIDE_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("archive drive type test override lock poisoned");
        if configured
            .as_ref()
            .is_some_and(|(expected, _)| expected == path)
        {
            configured
                .take()
                .map_or(actual, |(_, drive_type)| drive_type)
        } else {
            actual
        }
    }

    #[cfg(test)]
    pub(super) fn set_archive_drive_type_override_for_test(path: &Path, drive_type: u32) {
        *ARCHIVE_DRIVE_TYPE_OVERRIDE_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("archive drive type test override lock poisoned") =
            Some((path.to_path_buf(), drive_type));
    }

    #[cfg(test)]
    pub(super) fn archive_drive_type_override_pending_for_test(path: &Path) -> bool {
        ARCHIVE_DRIVE_TYPE_OVERRIDE_FOR_TEST
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("archive drive type test override lock poisoned")
            .as_ref()
            .is_some_and(|(expected, _)| expected == path)
    }

    #[cfg(test)]
    pub(super) fn archive_volume_info_for_test(path: &Path) -> Result<(u32, String), CopyError> {
        let mut drive = path
            .components()
            .next()
            .ok_or(CopyError::Unsupported)?
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>();
        drive.extend([b'\\' as u16, 0]);
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) };
        let mut filesystem = [0u16; 32];
        unsafe {
            GetVolumeInformationW(
                PCWSTR(drive.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
            .map_err(io)?;
        }
        let filesystem = String::from_utf16_lossy(
            &filesystem[..filesystem
                .iter()
                .position(|u| *u == 0)
                .unwrap_or(filesystem.len())],
        );
        Ok((drive_type, filesystem))
    }

    #[cfg(test)]
    mod archive_volume_policy_tests {
        use super::archive_source_volume_allowed;

        #[test]
        fn archive_policy_accepts_only_local_ntfs_or_exfat_volumes() {
            for drive_type in [2, 3] {
                assert!(archive_source_volume_allowed(drive_type, "NTFS"));
                assert!(archive_source_volume_allowed(drive_type, "ntfs"));
                assert!(archive_source_volume_allowed(drive_type, "exFAT"));
                assert!(archive_source_volume_allowed(drive_type, "EXFAT"));
            }
            for drive_type in [0, 1, 4, 5, 6, u32::MAX] {
                assert!(!archive_source_volume_allowed(drive_type, "NTFS"));
                assert!(!archive_source_volume_allowed(drive_type, "exFAT"));
            }
            for drive_type in [2, 3] {
                for filesystem in ["FAT32", "ReFS", "unknown"] {
                    assert!(!archive_source_volume_allowed(drive_type, filesystem));
                }
            }
        }
    }

    pub(super) fn open_pinned_directory(
        path: &Path,
        delete_capable: bool,
    ) -> Result<DirectoryLock, CopyError> {
        open_pinned_directory_with_share(path, delete_capable, FILE_SHARE_READ)
    }

    pub(super) fn open_pinned_directory_with_share(
        path: &Path,
        delete_capable: bool,
        share: FILE_SHARE_MODE,
    ) -> Result<DirectoryLock, CopyError> {
        let wide_path = wide(path);
        let access = FILE_LIST_DIRECTORY.0
            | FILE_READ_ATTRIBUTES.0
            | if delete_capable { DELETE.0 } else { 0 };
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                access,
                share,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let lock = DirectoryLock(handle);
        directory_identity(&lock)?;
        Ok(lock)
    }

    #[cfg(test)]
    pub(super) fn open_add_children_ancestor_for_test(
        path: &Path,
    ) -> Result<DirectoryLock, CopyError> {
        let wide_path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                ::windows::Win32::Storage::FileSystem::FILE_ADD_FILE.0
                    | ::windows::Win32::Storage::FileSystem::FILE_ADD_SUBDIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        Ok(DirectoryLock(handle))
    }

    pub(super) fn open_pinned_destination_parent(path: &Path) -> Result<DirectoryLock, CopyError> {
        let wide_path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
                // Staging needs to add a sibling while this handle pins the destination route.
                // Omitting FILE_SHARE_DELETE still prevents replacing or removing that route.
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let directory = DirectoryLock(handle);
        directory_identity(&directory)?;
        Ok(directory)
    }

    #[cfg(test)]
    pub(super) fn open_pinned_destination_parent_for_test(
        path: &Path,
    ) -> Result<DirectoryLock, CopyError> {
        open_pinned_destination_parent(path)
    }

    #[repr(C)]
    struct NtUnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct NtObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut NtUnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }
    #[repr(C)]
    union NtIoStatusValue {
        status: i32,
        pointer: *mut c_void,
    }
    #[repr(C)]
    struct NtIoStatusBlock {
        status: NtIoStatusValue,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *mut NtObjectAttributes,
            io_status_block: *mut NtIoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    const NT_FILE_CREATE: u32 = 2;
    const NT_FILE_OPEN: u32 = 1;
    const NT_FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
    const NT_FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    const NT_FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const NT_FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const NT_OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
    const STAGING_DIRECTORY_ACCESS: u32 = 0x0000_0001 // FILE_LIST_DIRECTORY
        | 0x0000_0002 // FILE_ADD_FILE
        | 0x0000_0004 // FILE_ADD_SUBDIRECTORY
        | 0x0000_0080 // FILE_READ_ATTRIBUTES
        | 0x0010_0000; // SYNCHRONIZE
    const STAGING_DIRECTORY_READ_ACCESS: u32 = 0x0000_0001 // FILE_LIST_DIRECTORY
        | 0x0000_0080 // FILE_READ_ATTRIBUTES
        | 0x0010_0000; // SYNCHRONIZE

    fn open_staging_ancestor_chain(path: &Path) -> Result<Vec<DirectoryLock>, CopyError> {
        if !path.is_absolute() {
            return Err(CopyError::Unsupported);
        }
        let mut paths = path.ancestors().collect::<Vec<_>>();
        paths.reverse();
        let mut handles = Vec::with_capacity(paths.len().saturating_sub(1));
        for ancestor in paths.into_iter().take_while(|ancestor| *ancestor != path) {
            handles.push(open_pinned_directory(ancestor, false)?);
        }
        Ok(handles)
    }

    fn open_staging_directory(path: &Path) -> Result<DirectoryLock, CopyError> {
        let wide_path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                STAGING_DIRECTORY_ACCESS,
                // Allow parent-entry writes needed for the root's handle rename,
                // but never share delete while this ancestor chain pins the route.
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let directory = DirectoryLock(handle);
        directory_identity(&directory)?;
        Ok(directory)
    }

    pub(super) fn open_published_root(path: &Path) -> Result<DirectoryLock, CopyError> {
        let wide_path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                STAGING_DIRECTORY_READ_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let directory = DirectoryLock(handle);
        directory_identity(&directory)?;
        Ok(directory)
    }

    pub(super) fn create_attempt_staging_root(
        root_path: &Path,
    ) -> Result<StagingRootLease, CopyError> {
        create_attempt_staging_root_with_identity(root_path, directory_identity_text)
    }

    fn create_attempt_staging_root_with_identity(
        root_path: &Path,
        identity_of: impl FnOnce(&DirectoryLock) -> Result<String, CopyError>,
    ) -> Result<StagingRootLease, CopyError> {
        let parent_path = root_path.parent().ok_or(CopyError::Unsupported)?;
        let leaf = root_path
            .file_name()
            .ok_or(CopyError::Unsupported)?
            .encode_wide()
            .collect::<Vec<_>>();
        if !root_path.is_absolute() {
            return Err(CopyError::Unsupported);
        }
        ensure_local_fixed_ntfs(parent_path)?;
        let mut root_chain = open_staging_ancestor_chain(parent_path)?;
        let parent = open_staging_directory(parent_path)?;
        root_chain.push(parent);
        let root = DirectoryLock(nt_create_child(
            root_chain.last().ok_or(CopyError::Unsupported)?,
            &leaf,
            true,
            true,
            FILE_SHARE_READ.0,
        )?);
        let root_identity = match identity_of(&root) {
            Ok(identity) => identity,
            Err(original) => {
                let cleanup = match enumerate_directory_handle(&root, 1, 2) {
                    Ok(entries) if entries.is_empty() => set_raw_handle_delete_disposition(root.0),
                    Ok(_) => Err(CopyError::Unsupported),
                    Err(error) => Err(error),
                };
                drop(root);
                drop(root_chain);
                if cleanup.is_ok() && !root_path.exists() {
                    return Err(original);
                }
                return Err(staging_cleanup_partial(
                    root_path,
                    original,
                    cleanup.err().unwrap_or(CopyError::Unsupported),
                ));
            }
        };
        Ok(StagingRootLease {
            root_path: root_path.to_path_buf(),
            root_identity,
            root_chain,
            root,
        })
    }

    #[cfg(test)]
    pub(super) fn create_attempt_staging_root_with_identity_failure_for_test(
        root_path: &Path,
    ) -> Result<StagingRootLease, CopyError> {
        create_attempt_staging_root_with_identity(root_path, |_| {
            Err(CopyError::Io("injected staging identity failure".into()))
        })
    }

    pub(super) fn publish_and_verify_staged_tree(
        staged: VerifiedStagedTree,
        destination: &Path,
        cancelled: &AtomicBool,
    ) -> Result<VerifiedPublishedTree, PublishTreeFailure> {
        publish_and_verify_staged_tree_with_hooks_mode(
            staged,
            destination,
            cancelled,
            || {},
            open_published_root,
            |_| {},
            None,
        )
    }

    pub(super) fn publish_and_verify_staged_tree_for_undo(
        staged: VerifiedStagedTree,
        destination: &Path,
        cancelled: &AtomicBool,
    ) -> Result<VerifiedPublishedTree, UndoTreePublishFailure> {
        let mut custody = None;
        let result = publish_and_verify_staged_tree_with_hooks_mode(
            staged, destination, cancelled, || {}, open_published_root, |_| {}, Some(&mut custody),
        );
        match result {
            Err(PublishTreeFailure::BeforePublish(CopyError::DestinationExists)) if custody.is_some() => {
                Err(UndoTreePublishFailure::RetryableCollision(custody.take().unwrap()))
            }
            Ok(published) => Ok(published),
            Err(PublishTreeFailure::BeforePublish(error)) => Err(UndoTreePublishFailure::BeforePublish(error)),
            Err(PublishTreeFailure::AfterPublish { error, .. }) => Err(UndoTreePublishFailure::AfterPublish { error }),
        }
    }

    fn clean_staged_failure_from_facts_error(
        staged: VerifiedStagedTree,
        original: CopyError,
    ) -> CopyError {
        match clean_staged_failure_from_facts(staged, original) {
            PublishTreeFailure::BeforePublish(error) => error,
            PublishTreeFailure::AfterPublish { error, .. } => error,
        }
    }

    pub(super) fn publish_and_verify_staged_tree_with_hooks(
        staged: VerifiedStagedTree,
        destination: &Path,
        cancelled: &AtomicBool,
        after_publish: impl FnOnce(),
        open_root: impl FnMut(&Path) -> Result<DirectoryLock, CopyError>,
        after_child_open: impl FnMut(bool),
    ) -> Result<VerifiedPublishedTree, PublishTreeFailure> {
        publish_and_verify_staged_tree_with_hooks_mode(staged, destination, cancelled, after_publish, open_root, after_child_open, None)
    }

    fn publish_and_verify_staged_tree_with_hooks_mode(
        mut staged: VerifiedStagedTree,
        destination: &Path,
        cancelled: &AtomicBool,
        after_publish: impl FnOnce(),
        mut open_root: impl FnMut(&Path) -> Result<DirectoryLock, CopyError>,
        mut after_child_open: impl FnMut(bool),
        mut retryable_collision_custody: Option<&mut Option<VerifiedStagedTree>>,
    ) -> Result<VerifiedPublishedTree, PublishTreeFailure> {
        if cancelled.load(Ordering::Acquire)
            || staged.root_path.parent() != destination.parent()
            || !destination.is_absolute()
            || destination.file_name().is_none()
        {
            return Err(clean_staged_failure(
                staged,
                if cancelled.load(Ordering::Acquire) {
                    CopyError::Cancelled
                } else {
                    CopyError::Unsupported
                },
            ));
        }
        let root_identity = match directory_identity(&staged.root) {
            Ok(identity) => identity,
            Err(error) => return Err(clean_staged_failure(staged, error)),
        };
        let root_identity_text = format!(
            "{}:{:08x}{:08x}",
            root_identity.volume, root_identity.index_high, root_identity.index_low
        );
        if root_identity_text != staged.root_identity {
            return Err(clean_staged_failure(staged, CopyError::Unsupported));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(clean_staged_failure(staged, CopyError::Cancelled));
        }
        // The held child handles deny writes but also prevent Windows from
        // renaming their parent directory. Preserve their exact facts, then
        // release just these child handles immediately before the root rename.
        drop(std::mem::take(&mut staged.handles));
        #[cfg(test)]
        if retryable_collision_custody.is_some() {
            run_cross_volume_directory_undo_before_publish_hook_for_test(destination);
        }
        #[cfg(test)]
        let publish_result = if retryable_collision_custody.is_some()
            && take_cross_volume_directory_undo_access_denied_for_test(destination)
        {
            Err(CopyError::AccessDenied)
        } else {
            publish_no_replace_handle(staged.root.0, destination)
        };
        #[cfg(not(test))]
        let publish_result = publish_no_replace_handle(staged.root.0, destination);
        if let Err(error) = publish_result {
            let destination_matches = open_root(destination)
                .and_then(|handle| directory_identity_text(&handle))
                .is_ok_and(|identity| identity == staged.root_identity);
            if destination_matches {
                let partial = PartialPublishedTree {
                    root_path: destination.to_path_buf(),
                    root_identity: staged.root_identity,
                    root_chain: staged.root_chain,
                    root_publish_handle: staged.root,
                    reopened_root: None,
                    nodes: Vec::new(),
                    _staged_handles: staged.handles,
                    pending_node: None,
                };
                return Err(PublishTreeFailure::AfterPublish {
                    error: CopyError::Partial(format!(
                        "publish reported an error but destination has the exact staged root identity: {error:?}"
                    )),
                    tree: Box::new(partial),
                });
            }
            let stage_matches = open_root(&staged.root_path)
                .and_then(|handle| directory_identity_text(&handle))
                .is_ok_and(|identity| identity == staged.root_identity);
            if stage_matches && matches!(error, CopyError::DestinationExists) {
                if let Some(custody) = retryable_collision_custody.as_deref_mut() {
                    *custody = Some(staged);
                    return Err(PublishTreeFailure::BeforePublish(CopyError::DestinationExists));
                }
            }
            if stage_matches
                && matches!(error, CopyError::AccessDenied)
                && retryable_collision_custody.is_some()
                && open_staged_child_directory_identity(&staged, destination)
                    .is_ok_and(|identity| identity != staged.root_identity)
            {
                return Err(clean_staged_failure_from_facts(staged, error));
            }
            if stage_matches && !destination.exists() {
                return Err(clean_staged_failure_from_facts(
                    staged,
                    CopyError::Io(format!("publish exact staging root handle: {error:?}")),
                ));
            }
            let partial = PartialPublishedTree {
                root_path: destination.to_path_buf(),
                root_identity: staged.root_identity,
                root_chain: staged.root_chain,
                root_publish_handle: staged.root,
                reopened_root: None,
                nodes: Vec::new(),
                _staged_handles: staged.handles,
                pending_node: None,
            };
            return Err(PublishTreeFailure::AfterPublish {
                error: CopyError::Partial(format!(
                    "publish outcome is ambiguous; destination {}; staging path {}; source retained: {error:?}",
                    destination.display(), staged.root_path.display()
                )),
                tree: Box::new(partial),
            });
        }

        let mut partial = PartialPublishedTree {
            root_path: destination.to_path_buf(),
            root_identity: staged.root_identity,
            root_chain: staged.root_chain,
            root_publish_handle: staged.root,
            reopened_root: None,
            nodes: Vec::with_capacity(staged.nodes.len()),
            _staged_handles: staged.handles,
            pending_node: None,
        };
        after_publish();
        let result = verify_published_tree_after_publish(
            &staged.nodes,
            root_identity,
            cancelled,
            open_root,
            &mut after_child_open,
            &mut partial,
        );
        match result {
            Ok(()) if partial.pending_node.is_none() => match partial.reopened_root.take() {
                Some(root) => Ok(VerifiedPublishedTree {
                    root_path: partial.root_path,
                    root_identity: partial.root_identity,
                    root_chain: partial.root_chain,
                    root_publish_handle: partial.root_publish_handle,
                    root,
                    nodes: partial.nodes,
                }),
                None => Err(PublishTreeFailure::AfterPublish {
                    error: CopyError::Unsupported,
                    tree: Box::new(partial),
                }),
            },
            Ok(()) => Err(PublishTreeFailure::AfterPublish {
                error: CopyError::Unsupported,
                tree: Box::new(partial),
            }),
            Err(error) => Err(PublishTreeFailure::AfterPublish {
                error,
                tree: Box::new(partial),
            }),
        }
    }

    fn verify_published_tree_after_publish(
        facts: &[StagedChildFact],
        root_identity: DirectoryIdentity,
        cancelled: &AtomicBool,
        open_root: impl FnOnce(&Path) -> Result<DirectoryLock, CopyError>,
        after_child_open: &mut impl FnMut(bool),
        partial: &mut PartialPublishedTree,
    ) -> Result<(), CopyError> {
        use std::io::{Seek, SeekFrom};
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }

        let published_root = open_root(&partial.root_path)
            .map_err(|error| CopyError::Io(format!("reopen published root: {error:?}")))?;
        partial.reopened_root = Some(published_root);
        if directory_identity(
            partial
                .reopened_root
                .as_ref()
                .ok_or(CopyError::Unsupported)?,
        )? != root_identity
        {
            return Err(CopyError::Unsupported);
        }
        let mut order: Vec<_> = (0..facts.len()).collect();
        order.sort_by_key(|index| facts[*index].relative_components.len());
        let mut buffer = [0u8; 64 * 1024];
        use sha2::Digest;
        for index in order {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            let fact = &facts[index];
            let parent_depth = fact.relative_components.len() - 1;
            let parent = if parent_depth == 0 {
                partial
                    .reopened_root
                    .as_ref()
                    .ok_or(CopyError::Unsupported)?
            } else {
                partial
                    .nodes
                    .iter()
                    .find(|node| {
                        node.node_kind
                            == crate::operations::journal::JournalDirectoryNodeKind::Directory
                            && node.relative_components.len() == parent_depth
                            && node.relative_components == fact.relative_components[..parent_depth]
                    })
                    .map(|node| match &node.handle {
                        PinnedSourceHandle::Directory(directory) => directory,
                        PinnedSourceHandle::File(_) => unreachable!("directory parent validated"),
                    })
                    .ok_or(CopyError::Unsupported)?
            };
            let component = fact
                .relative_components
                .last()
                .ok_or(CopyError::Unsupported)?;
            match fact.node_kind {
                crate::operations::journal::JournalDirectoryNodeKind::Directory => {
                    let directory =
                        DirectoryLock(nt_open_child(parent, component, true).map_err(|error| {
                            CopyError::Io(format!(
                                "reopen published directory {component:?}: {error:?}"
                            ))
                        })?);
                    partial.pending_node = Some(UnverifiedPublishedNode {
                        relative_components: fact.relative_components.clone(),
                        node_kind: fact.node_kind,
                        identity: fact.identity.clone(),
                        content_fact: fact.content_fact.clone(),
                        handle: PinnedSourceHandle::Directory(directory),
                    });
                    after_child_open(false);
                    let PinnedSourceHandle::Directory(directory) = &partial
                        .pending_node
                        .as_ref()
                        .ok_or(CopyError::Unsupported)?
                        .handle
                    else {
                        return Err(CopyError::Unsupported);
                    };
                    let identity = directory_identity(directory)?;
                    let identity_text = format!(
                        "{}:{:08x}{:08x}",
                        identity.volume, identity.index_high, identity.index_low
                    );
                    if identity_text != fact.identity || fact.content_fact != "directory" {
                        return Err(CopyError::Unsupported);
                    }
                }
                crate::operations::journal::JournalDirectoryNodeKind::File => {
                    use std::os::windows::io::FromRawHandle;
                    let file = unsafe {
                        std::fs::File::from_raw_handle(
                            nt_open_child(parent, component, false)
                                .map_err(|error| {
                                    CopyError::Io(format!(
                                        "reopen published file {component:?}: {error:?}"
                                    ))
                                })?
                                .0,
                        )
                    };
                    partial.pending_node = Some(UnverifiedPublishedNode {
                        relative_components: fact.relative_components.clone(),
                        node_kind: fact.node_kind,
                        identity: fact.identity.clone(),
                        content_fact: fact.content_fact.clone(),
                        handle: PinnedSourceHandle::File(file),
                    });
                    after_child_open(true);
                    let PinnedSourceHandle::File(file) = &mut partial
                        .pending_node
                        .as_mut()
                        .ok_or(CopyError::Unsupported)?
                        .handle
                    else {
                        return Err(CopyError::Unsupported);
                    };
                    let identity = regular_file_identity(&file)?;
                    let identity_text = format!(
                        "{}:{:08x}{:08x}",
                        identity.volume, identity.index_high, identity.index_low
                    );
                    if identity_text != fact.identity {
                        return Err(CopyError::Unsupported);
                    }
                    file.seek(SeekFrom::Start(0)).map_err(io)?;
                    let mut hasher = sha2::Sha256::new();
                    loop {
                        if cancelled.load(Ordering::Acquire) {
                            return Err(CopyError::Cancelled);
                        }
                        let read = file_read_chunk(&file, &mut buffer).map_err(io)?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    if format!("sha256:{:x}", hasher.finalize()) != fact.content_fact {
                        return Err(CopyError::Unsupported);
                    }
                }
                _ => return Err(CopyError::Unsupported),
            }
            let verified = partial.pending_node.take().ok_or(CopyError::Unsupported)?;
            partial.nodes.push(VerifiedStagedNode {
                relative_components: verified.relative_components,
                node_kind: verified.node_kind,
                identity: verified.identity,
                content_fact: verified.content_fact,
                handle: verified.handle,
            });
        }

        verify_exact_published_children(&partial.root_path, facts).map_err(|error| {
            CopyError::Io(format!(
                "verify exact published directory contents: {error:?}"
            ))
        })?;
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        if directory_identity(
            partial
                .reopened_root
                .as_ref()
                .ok_or(CopyError::Unsupported)?,
        )? != root_identity
        {
            return Err(CopyError::Unsupported);
        }
        for node in &partial.nodes {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            let identity = match &node.handle {
                PinnedSourceHandle::Directory(directory) => {
                    let id = directory_identity(directory)?;
                    format!("{}:{:08x}{:08x}", id.volume, id.index_high, id.index_low)
                }
                PinnedSourceHandle::File(file) => {
                    let id = regular_file_identity(file)?;
                    format!("{}:{:08x}{:08x}", id.volume, id.index_high, id.index_low)
                }
            };
            if identity != node.identity {
                return Err(CopyError::Unsupported);
            }
        }
        Ok(())
    }

    fn file_read_chunk(file: &std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Read;
        let mut file = file;
        file.read(buffer)
    }

    fn verify_exact_published_children(
        root_path: &Path,
        facts: &[StagedChildFact],
    ) -> Result<(), CopyError> {
        use std::{
            collections::HashSet,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        let mut directories = vec![Vec::<Vec<u16>>::new()];
        directories.extend(facts.iter().filter_map(|fact| {
            (fact.node_kind == crate::operations::journal::JournalDirectoryNodeKind::Directory)
                .then(|| fact.relative_components.clone())
        }));
        for directory in directories {
            let mut path = root_path.to_path_buf();
            for component in &directory {
                path.push(std::ffi::OsString::from_wide(component));
            }
            let expected: HashSet<Vec<u16>> = facts
                .iter()
                .filter_map(|fact| {
                    (fact.relative_components.len() == directory.len() + 1
                        && fact.relative_components[..directory.len()] == directory)
                        .then(|| fact.relative_components[directory.len()].clone())
                })
                .collect();
            let mut actual = HashSet::new();
            for child in std::fs::read_dir(&path).map_err(io)? {
                let child = child.map_err(io)?;
                let name = child.file_name().encode_wide().collect::<Vec<_>>();
                let metadata = std::fs::symlink_metadata(child.path()).map_err(io)?;
                if metadata.file_type().is_symlink() || !actual.insert(name) {
                    return Err(CopyError::Unsupported);
                }
            }
            if actual != expected {
                return Err(CopyError::Unsupported);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn staged_identities_for_test(
        tree: &VerifiedStagedTree,
    ) -> Result<(String, Vec<(Vec<Vec<u16>>, String)>), CopyError> {
        if directory_identity_text(&tree.root)? != tree.root_identity {
            return Err(CopyError::Unsupported);
        }
        Ok((
            tree.root_identity.clone(),
            tree.nodes
                .iter()
                .map(|node| (node.relative_components.clone(), node.identity.clone()))
                .collect(),
        ))
    }

    #[cfg(test)]
    pub(super) fn verified_published_identities_for_test(
        tree: &VerifiedPublishedTree,
    ) -> Result<(String, Vec<(Vec<Vec<u16>>, String)>), CopyError> {
        let root = directory_identity(&tree.root)?;
        let root_identity = format!(
            "{}:{:08x}{:08x}",
            root.volume, root.index_high, root.index_low
        );
        let publish_root = directory_identity(&tree.root_publish_handle)?;
        let publish_root_identity = format!(
            "{}:{:08x}{:08x}",
            publish_root.volume, publish_root.index_high, publish_root.index_low
        );
        if root_identity != publish_root_identity || root_identity != tree.root_identity {
            return Err(CopyError::Unsupported);
        }
        let nodes = tree
            .nodes
            .iter()
            .map(|node| {
                let identity = match &node.handle {
                    PinnedSourceHandle::Directory(directory) => {
                        let id = directory_identity(directory)?;
                        format!("{}:{:08x}{:08x}", id.volume, id.index_high, id.index_low)
                    }
                    PinnedSourceHandle::File(file) => {
                        let id = regular_file_identity(file)?;
                        format!("{}:{:08x}{:08x}", id.volume, id.index_high, id.index_low)
                    }
                };
                Ok((node.relative_components.clone(), identity))
            })
            .collect::<Result<Vec<_>, CopyError>>()?;
        Ok((root_identity, nodes))
    }

    #[cfg(test)]
    pub(super) fn partial_published_root_identity_for_test(
        tree: &PartialPublishedTree,
    ) -> Result<String, CopyError> {
        let identity = directory_identity(&tree.root_publish_handle)?;
        Ok(format!(
            "{}:{:08x}{:08x}",
            identity.volume, identity.index_high, identity.index_low
        ))
    }

    fn nt_create_child(
        parent: &DirectoryLock,
        component: &[u16],
        directory: bool,
        delete_capable: bool,
        share_access: u32,
    ) -> Result<HANDLE, CopyError> {
        nt_open_or_create_child(
            parent,
            component,
            directory,
            delete_capable,
            NT_FILE_CREATE,
            share_access,
            false,
        )
    }

    fn nt_create_7z_child(parent: &DirectoryLock, component: &[u16]) -> Result<HANDLE, CopyError> {
        nt_open_or_create_child(
            parent,
            component,
            false,
            true,
            NT_FILE_CREATE,
            FILE_SHARE_READ.0,
            true,
        )
    }

    fn nt_open_child(
        parent: &DirectoryLock,
        component: &[u16],
        directory: bool,
    ) -> Result<HANDLE, CopyError> {
        nt_open_or_create_child(
            parent,
            component,
            directory,
            false,
            NT_FILE_OPEN,
            FILE_SHARE_READ.0,
            false,
        )
    }

    enum NtChildProbe {
        Absent,
        Present(HANDLE),
    }

    fn nt_probe_child_no_follow(
        parent: &DirectoryLock,
        component: &[u16],
    ) -> Result<NtChildProbe, CopyError> {
        const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xc000_0034;
        let name_bytes = component
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|bytes| u16::try_from(bytes).ok())
            .ok_or(CopyError::Unsupported)?;
        if component.is_empty()
            || component.iter().any(|unit| {
                *unit == 0 || *unit == b'/' as u16 || *unit == b'\\' as u16 || *unit == b':' as u16
            })
        {
            return Err(CopyError::Unsupported);
        }
        let mut name = component.to_vec();
        let mut unicode_name = NtUnicodeString {
            length: name_bytes,
            maximum_length: name_bytes,
            buffer: name.as_mut_ptr(),
        };
        let mut attributes = NtObjectAttributes {
            length: size_of::<NtObjectAttributes>() as u32,
            root_directory: parent.0,
            object_name: &mut unicode_name,
            attributes: NT_OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut status_block = NtIoStatusBlock {
            status: NtIoStatusValue { status: 0 },
            information: 0,
        };
        let mut handle = HANDLE::default();
        // 既存の葉だけを保持済みparent handleから非追跡で開き、pathの再解決を避ける。
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                0x0080 | 0x0010_0000, // FILE_READ_ATTRIBUTES | SYNCHRONIZE
                &mut attributes,
                &mut status_block,
                std::ptr::null(),
                0,
                FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0,
                NT_FILE_OPEN,
                NT_FILE_OPEN_REPARSE_POINT | NT_FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null_mut(),
                0,
            )
        };
        if status >= 0 {
            return Ok(NtChildProbe::Present(handle));
        }
        if status as u32 == STATUS_OBJECT_NAME_NOT_FOUND {
            return Ok(NtChildProbe::Absent);
        }
        let error = unsafe { RtlNtStatusToDosError(status) };
        Err(CopyError::Io(format!(
            "NtCreateFile child probe failed with NTSTATUS 0x{:08x}: {}",
            status as u32,
            std::io::Error::from_raw_os_error(error as i32)
        )))
    }

    fn nt_open_or_create_child(
        parent: &DirectoryLock,
        component: &[u16],
        directory: bool,
        delete_capable: bool,
        disposition: u32,
        share_access: u32,
        classify_create_collision: bool,
    ) -> Result<HANDLE, CopyError> {
        let name_bytes = component
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|bytes| u16::try_from(bytes).ok())
            .ok_or(CopyError::Unsupported)?;
        if component.is_empty()
            || component.iter().any(|unit| {
                *unit == 0 || *unit == b'/' as u16 || *unit == b'\\' as u16 || *unit == b':' as u16
            })
        {
            return Err(CopyError::Unsupported);
        }
        let mut name = component.to_vec();
        let mut unicode_name = NtUnicodeString {
            length: name_bytes,
            maximum_length: name_bytes,
            buffer: name.as_mut_ptr(),
        };
        let mut attributes = NtObjectAttributes {
            length: size_of::<NtObjectAttributes>() as u32,
            root_directory: parent.0,
            object_name: &mut unicode_name,
            attributes: NT_OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut status_block = NtIoStatusBlock {
            status: NtIoStatusValue { status: 0 },
            information: 0,
        };
        let mut handle = HANDLE::default();
        let desired_access = if directory {
            (if disposition == NT_FILE_CREATE {
                STAGING_DIRECTORY_ACCESS
            } else {
                STAGING_DIRECTORY_READ_ACCESS
            }) | if delete_capable { DELETE.0 } else { 0 }
        } else {
            (if disposition == NT_FILE_CREATE {
                GENERIC_READ.0 | GENERIC_WRITE.0
            } else {
                GENERIC_READ.0
            }) | if delete_capable { DELETE.0 } else { 0 }
                | 0x0010_0000 // SYNCHRONIZE
        };
        let options = NT_FILE_OPEN_REPARSE_POINT
            | NT_FILE_SYNCHRONOUS_IO_NONALERT
            | if directory {
                NT_FILE_DIRECTORY_FILE
            } else {
                NT_FILE_NON_DIRECTORY_FILE
            };
        // 既存のWin32 name-based openへ戻すと親pathを再解決するため、保持したdirectory handleをRootDirectoryに使う。
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access,
                &mut attributes,
                &mut status_block,
                std::ptr::null(),
                if directory {
                    FILE_ATTRIBUTE_DIRECTORY.0
                } else {
                    0x0000_0080
                },
                share_access,
                disposition,
                options,
                std::ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            if classify_create_collision && status as u32 == 0xc000_0035 {
                return Err(CopyError::DestinationExists);
            }
            let error = unsafe { RtlNtStatusToDosError(status) };
            return Err(CopyError::Io(format!(
                "NtCreateFile failed with NTSTATUS 0x{:08x} (directory={directory}, options=0x{options:08x}, access=0x{desired_access:08x}): {}",
                status as u32,
                std::io::Error::from_raw_os_error(error as i32)
            )));
        }
        Ok(handle)
    }

    pub(super) fn create_staged_directory(
        parent: &DirectoryLock,
        component: &[u16],
        delete_capable: bool,
        share_access: u32,
    ) -> Result<DirectoryLock, CopyError> {
        // The attempt root keeps DELETE on this exact handle for later rename-info publication.
        let directory = DirectoryLock(nt_create_child(
            parent,
            component,
            true,
            delete_capable,
            share_access,
        )?);
        directory_identity(&directory)?;
        Ok(directory)
    }

    pub(super) fn create_staged_file(
        parent: &DirectoryLock,
        component: &[u16],
    ) -> Result<std::fs::File, CopyError> {
        use std::os::windows::io::FromRawHandle;
        let handle = nt_create_child(parent, component, false, true, FILE_SHARE_READ.0)?;
        let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
        regular_file_identity(&file)?;
        Ok(file)
    }

    pub(super) fn create_7z_staged_file(
        parent: &DirectoryLock,
        component: &[u16],
    ) -> Result<std::fs::File, CopyError> {
        use std::os::windows::io::FromRawHandle;
        let handle = nt_create_7z_child(parent, component)?;
        let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
        regular_file_identity(&file)?;
        Ok(file)
    }

    pub(super) fn flush_staged_file(file: &std::fs::File) -> Result<(), CopyError> {
        unsafe { FlushFileBuffers(raw(file)).map_err(io) }
    }

    pub(super) fn open_pinned_file(path: &Path) -> Result<std::fs::File, CopyError> {
        open_pinned_file_with_access(path, true)
    }

    pub(super) fn open_pinned_file_with_access(
        path: &Path,
        delete_capable: bool,
    ) -> Result<std::fs::File, CopyError> {
        let access = if delete_capable {
            GENERIC_READ.0 | DELETE.0
        } else {
            GENERIC_READ.0
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(access)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .map_err(io)?;
        let identity = regular_file_identity(&file)?;
        if identity.volume == 0 {
            return Err(CopyError::Unsupported);
        }
        // FILE_SHARE_READ only makes CreateFileW fail when a conflicting write
        // or delete-capable open (including a writable mapping) already exists.
        Ok(file)
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_pinned_source_directory(
        directory_path: &Path,
        relative: Vec<Vec<u16>>,
        depth: usize,
        directory_handle_index: usize,
        handles: &mut Vec<Option<PinnedSourceHandle>>,
        entries: &mut Vec<PinnedSourceEntry>,
        name_bytes: &mut usize,
        cancelled: &AtomicBool,
        limits: &SourceTreeLimits,
        delete_capable: bool,
        hash_contents: bool,
        deadline: Option<Instant>,
    ) -> Result<(), CopyError> {
        use std::os::windows::ffi::OsStringExt;
        struct WalkFrame {
            directory_path: PathBuf,
            relative: Vec<Vec<u16>>,
            depth: usize,
            children: Vec<(Vec<u16>, bool)>,
            next_child: usize,
        }
        // 深い tree で worker stack を使い切らないよう、DFS の状態は heap に保持する。
        let first_children = enumerate_pinned_source_children(
            handles,
            directory_handle_index,
            entries.len(),
            *name_bytes,
            limits,
        )?;
        let mut frames = vec![WalkFrame {
            directory_path: directory_path.to_path_buf(),
            relative,
            depth,
            children: first_children,
            next_child: 0,
        }];

        while !frames.is_empty() {
            let (directory_path, relative, depth, component, is_directory) = {
                let frame = frames.last_mut().expect("nonempty walk stack");
                if frame.next_child >= frame.children.len() {
                    frames.pop();
                    continue;
                }
                let (component, is_directory) = frame.children[frame.next_child].clone();
                frame.next_child += 1;
                (
                    frame.directory_path.clone(),
                    frame.relative.clone(),
                    frame.depth,
                    component,
                    is_directory,
                )
            };
            if cancelled.load(Ordering::Acquire)
                || deadline.is_some_and(|limit| Instant::now() >= limit)
            {
                return Err(CopyError::Cancelled);
            }
            if component.is_empty() || component.iter().any(|unit| *unit == 0) {
                return Err(CopyError::Unsupported);
            }
            let name = std::ffi::OsString::from_wide(&component);
            if relative.len().saturating_add(1) > limits.depth {
                return Err(CopyError::Unsupported);
            }
            let relative_name_bytes = if limits.zip_utf8_name_bytes {
                relative
                    .iter()
                    .chain(std::iter::once(&component))
                    .try_fold(relative.len(), |sum, part| {
                        let text = String::from_utf16(part).map_err(|_| CopyError::Unsupported)?;
                        sum.checked_add(text.len()).ok_or(CopyError::Unsupported)
                    })?
                    .saturating_add(usize::from(is_directory))
            } else {
                relative
                    .iter()
                    .chain(std::iter::once(&component))
                    .map(|part| part.len().saturating_mul(2))
                    .sum::<usize>()
            };
            *name_bytes = name_bytes
                .checked_add(
                    limits
                        .emitted_name_prefix_bytes
                        .saturating_add(relative_name_bytes),
                )
                .ok_or(CopyError::Unsupported)?;
            if *name_bytes > limits.relative_name_bytes {
                return Err(CopyError::Unsupported);
            }
            if entries.len() >= limits.entries {
                return Err(CopyError::Unsupported);
            }
            let mut child_relative = relative.clone();
            child_relative.push(component.clone());
            let path = directory_path.join(&name);
            let (kind, identity, content_fact, recurse, handle) = if is_directory {
                let pinned = open_pinned_directory(&path, delete_capable)?;
                let identity = directory_identity(&pinned)?;
                let text = format!(
                    "{}:{:08x}{:08x}",
                    identity.volume, identity.index_high, identity.index_low
                );
                (
                    crate::operations::journal::JournalDirectoryNodeKind::Directory,
                    text,
                    "directory".to_string(),
                    true,
                    PinnedSourceHandle::Directory(pinned),
                )
            } else {
                let mut file = open_pinned_file_with_access(&path, delete_capable)?;
                let identity = regular_file_identity(&file)?;
                let content_fact = if hash_contents {
                    use sha2::Digest;
                    let mut hasher = sha2::Sha256::new();
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        if cancelled.load(Ordering::Acquire)
                            || deadline.is_some_and(|limit| Instant::now() >= limit)
                        {
                            return Err(CopyError::Cancelled);
                        }
                        let read = file.read(&mut buffer).map_err(io)?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    format!("sha256:{:x}", hasher.finalize())
                } else {
                    "not-hashed-for-zip".to_string()
                };
                let text = format!(
                    "{}:{:08x}{:08x}",
                    identity.volume, identity.index_high, identity.index_low
                );
                (
                    crate::operations::journal::JournalDirectoryNodeKind::File,
                    text,
                    content_fact,
                    false,
                    PinnedSourceHandle::File(file),
                )
            };
            let handle_index = handles.len();
            handles.push(Some(handle));
            entries.push(PinnedSourceEntry {
                relative_components: child_relative.clone(),
                node_kind: kind,
                source_identity: identity,
                content_fact,
                handle_index,
            });
            // Reserve bounded space for the attempt envelope, journal paths,
            // and deletion ledger that will accompany this manifest later.
            const RECORD_ENVELOPE_RESERVE: usize = 64 * 1024;
            let manifest_bytes = serde_json::to_vec(entries).map_err(io)?.len();
            if manifest_bytes
                .saturating_mul(2)
                .saturating_add(RECORD_ENVELOPE_RESERVE)
                > limits.record_bytes
            {
                return Err(CopyError::Unsupported);
            }
            if recurse {
                // The directory remains open without write/delete sharing while
                // its one-component descendants are enumerated by path.
                // All ancestor handles stay pinned at their retained indices
                // while one-component descendants are opened by path.
                let children = enumerate_pinned_source_children(
                    handles,
                    handle_index,
                    entries.len(),
                    *name_bytes,
                    limits,
                )?;
                frames.push(WalkFrame {
                    directory_path: path,
                    relative: child_relative,
                    depth: depth + 1,
                    children,
                    next_child: 0,
                });
            }
        }
        Ok(())
    }

    fn enumerate_pinned_source_children(
        handles: &[Option<PinnedSourceHandle>],
        directory_handle_index: usize,
        entry_count: usize,
        used_name_bytes: usize,
        limits: &SourceTreeLimits,
    ) -> Result<Vec<(Vec<u16>, bool)>, CopyError> {
        match handles.get(directory_handle_index).and_then(Option::as_ref) {
            Some(PinnedSourceHandle::Directory(directory)) => enumerate_directory_handle(
                directory,
                limits.entries.saturating_sub(entry_count),
                if limits.zip_utf8_name_bytes {
                    limits
                        .relative_name_bytes
                        .saturating_sub(used_name_bytes)
                        .saturating_mul(2)
                } else {
                    limits.relative_name_bytes.saturating_sub(used_name_bytes)
                },
            ),
            _ => Err(CopyError::Unsupported),
        }
    }

    /// Keeps the exact conflicting regular file and every ancestor used to
    /// resolve it alive while the conflict dialog is open.  The target is
    /// opened without FILE_SHARE_DELETE, so a later Replace answer cannot be
    /// redirected to a path-reused object.
    pub(super) struct ReplaceTarget {
        file: std::fs::File,
        _ancestors: Vec<DirectoryLock>,
    }

    /// The source approved for Replace.  It is held from conflict observation
    /// through staging, so a path reuse while the dialog is open cannot turn a
    /// copy of A into a replacement with unrelated B.
    pub(super) struct ReplaceSource {
        file: std::fs::File,
        _ancestors: Vec<DirectoryLock>,
    }

    pub(super) fn prepare_regular_file_replace(
        destination: &Path,
    ) -> Result<ReplaceTarget, CopyError> {
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let ancestors = lock_non_reparse_ancestors(parent)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(DELETE.0)
            // Replace対象もpin後にwriter/mappingが変更できないようにする。
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(destination)
            .map_err(io)?;
        regular_file_identity(&file)?;
        Ok(ReplaceTarget {
            file,
            _ancestors: ancestors,
        })
    }

    pub(super) fn prepare_regular_file_source(source: &Path) -> Result<ReplaceSource, CopyError> {
        let parent = source.parent().ok_or(CopyError::Unsupported)?;
        let ancestors = lock_non_reparse_ancestors(parent)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(source)
            .map_err(io)?;
        regular_file_identity(&file)?;
        Ok(ReplaceSource {
            file,
            _ancestors: ancestors,
        })
    }

    pub(super) fn prepare_regular_file_move_source(
        source: &Path,
    ) -> Result<ReplaceSource, CopyError> {
        let parent = source.parent().ok_or(CopyError::Unsupported)?;
        let ancestors = lock_non_reparse_ancestors(parent)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(DELETE.0 | GENERIC_READ.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(source)
            .map_err(io)?;
        regular_file_identity(&file)?;
        Ok(ReplaceSource {
            file,
            _ancestors: ancestors,
        })
    }

    pub(super) fn cleanup_external_move_source(
        source: &Path,
        expected: &crate::filesystem_identity::FilesystemIdentity,
        expected_route: &[crate::operations::model::SourceCleanupParentIdentity],
        expected_signature: &crate::operations::model::SourceCleanupLeafSignature,
        original: std::fs::File,
        cancelled: &AtomicBool,
    ) -> Result<(), &'static str> {
        cleanup_external_move_source_with_before_disposition(
            source,
            expected,
            expected_route,
            expected_signature,
            original,
            cancelled,
            || {},
        )
    }

    pub(super) fn cleanup_external_move_source_with_before_disposition(
        source: &Path,
        expected: &crate::filesystem_identity::FilesystemIdentity,
        expected_route: &[crate::operations::model::SourceCleanupParentIdentity],
        expected_signature: &crate::operations::model::SourceCleanupLeafSignature,
        original: std::fs::File,
        cancelled: &AtomicBool,
        before_disposition: impl FnOnce(),
    ) -> Result<(), &'static str> {
        let (volume, index) = expected
            .windows_parts()
            .ok_or("invalid captured identity")?;
        let original_identity = regular_file_identity(&original)
            .map_err(|_| "original source lease is not a regular file")?;
        let original_index = (u64::from(original_identity.index_high) << 32)
            | u64::from(original_identity.index_low);
        if original_identity.volume != volume || original_index != index {
            return Err("original source lease identity changed");
        }
        let parent = source.parent().ok_or("source has no parent")?;
        let ancestors =
            lock_non_reparse_ancestors(parent).map_err(|_| "source parent unavailable")?;
        if ancestors.len() != expected_route.len()
            || ancestors
                .iter()
                .rev()
                .zip(expected_route)
                .any(|(locked, captured)| {
                    directory_identity(locked).ok().is_none_or(|actual| {
                        captured.identity.windows_parts()
                            != Some((
                                actual.volume,
                                (u64::from(actual.index_high) << 32) | u64::from(actual.index_low),
                            ))
                    })
                })
        {
            return Err("source parent route changed; source retained");
        }
        let file = std::fs::OpenOptions::new()
            .access_mode(DELETE.0)
            // Keep this identity-matched object stable through disposition:
            // no concurrent writer or delete/rename opener may enter after
            // the original drag lease is released.
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(source)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "source is absent"
                } else {
                    "source could not be opened safely"
                }
            })?;
        let actual = regular_file_identity(&file).map_err(|_| "source is not a regular file")?;
        let actual_index = (u64::from(actual.index_high) << 32) | u64::from(actual.index_low);
        if actual.volume != volume || actual_index != index {
            return Err("source identity changed; replacement retained");
        }
        let actual_signature = leaf_signature(&file)
            .map_err(|_| "source link metadata is unavailable; source retained")?;
        if &actual_signature != expected_signature {
            return Err("source link metadata changed; source retained");
        }
        // The original handle prevents file-ID reuse until the path handle is
        // open and compared. Disposition must target only the current path.
        drop(original);
        before_disposition();
        if cancelled.load(Ordering::Acquire) {
            return Err("cancelled");
        }
        delete_exact_source(&file).map_err(|_| "exact source disposition failed")
    }

    impl ReplaceTarget {
        pub(super) fn displace_to(&self, backup: &Path) -> Result<(), CopyError> {
            publish_no_replace_handle(raw(&self.file), backup)
        }

        pub(super) fn delete_backup(&self) -> Result<(), CopyError> {
            delete_exact_source(&self.file)
        }

        fn identity(&self) -> Result<RegularFileIdentity, CopyError> {
            regular_file_identity(&self.file)
        }
    }

    impl ReplaceSource {
        fn identity(&self) -> Result<RegularFileIdentity, CopyError> {
            regular_file_identity(&self.file)
        }

        pub(super) fn shared_identity(
            &self,
        ) -> Option<crate::filesystem_identity::FilesystemIdentity> {
            regular_file_identity(&self.file)
                .ok()
                .map(|identity| identity.shared())
        }

        pub(super) fn same_volume_as(&self, target: &ReplaceTarget) -> bool {
            self.identity()
                .ok()
                .zip(target.identity().ok())
                .is_some_and(|(source, destination)| source.volume == destination.volume)
        }

        pub(super) fn same_object_as(&self, target: &ReplaceTarget) -> bool {
            self.identity()
                .ok()
                .zip(target.identity().ok())
                .is_some_and(|(source, destination)| source == destination)
        }
    }

    pub(super) struct ReplaceAttempt {
        pub receipt: ReplaceReceipt,
        pub detail: Option<String>,
    }

    /// The pre-opened `target` is the object the dialog actually approved.
    /// Nothing is displaced until the Shell has durably journaled `backup` and
    /// acknowledged it on this exact worker channel.
    pub(super) fn replace_regular_file(
        source: &Path,
        source_handle: ReplaceSource,
        destination: &Path,
        target: ReplaceTarget,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        event: &mut dyn FnMut(ReplaceEvent),
        after_displace: &mut dyn FnMut(),
        on_copy_undo: &mut dyn FnMut(crate::operations::model::CopyUndoReceipt),
    ) -> Result<ReplaceAttempt, CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let (output, content_sha256, _) = stage_open_regular_file(
            &source_handle.file,
            destination,
            cancelled,
            &mut |bytes| event(ReplaceEvent::BytesCopied(bytes)),
            true,
            &mut |_, _| {},
        )?;
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let mut backup_suffix = [0u8; 16];
        if let Err(error) = getrandom::fill(&mut backup_suffix) {
            let _ = cleanup_owned(&output);
            return Err(CopyError::Io(format!(
                "generate Replace backup identity: {error}"
            )));
        }
        let backup = parent.join(format!(
            ".gpui-filer-replace-{}-{}-{}",
            token.job_id().value(),
            token.attempt_id().value(),
            format!("{:032x}", u128::from_be_bytes(backup_suffix))
        ));
        if backup.exists() {
            let _ = cleanup_owned(&output);
            return Err(CopyError::Unsupported);
        }
        event(ReplaceEvent::BackupIntent(
            ExecutorFact::ReplaceBackupIntent {
                token,
                item,
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
                backup: backup.clone(),
            },
        ));
        match controls.recv() {
            Ok(WorkerControl::ReplaceBackupJournaledExact {
                token: actual_token,
                item: actual_item,
                source: actual_source,
                destination: actual_destination,
                backup: actual_backup,
            }) if actual_token == token
                && actual_item == item
                && actual_source == source
                && actual_destination == destination
                && actual_backup == backup => {}
            Ok(WorkerControl::Cancel) | Err(_) => {
                let _ = cleanup_owned(&output);
                return Err(CopyError::Cancelled);
            }
            Ok(_) => {
                let _ = cleanup_owned(&output);
                return Err(CopyError::Unsupported);
            }
        }
        if cancelled.load(Ordering::Acquire) {
            let _ = cleanup_owned(&output);
            return Err(CopyError::Cancelled);
        }
        if let Err(error) = target.displace_to(&backup) {
            let _ = cleanup_owned(&output);
            return Err(error);
        }
        after_displace();
        let mut receipt = ReplaceReceipt {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            displaced_backup: Some(backup.clone()),
            destination_published: false,
            displaced_backup_removed: false,
        };
        if cancelled.load(Ordering::Acquire) {
            let _ = cleanup_owned(&output);
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("destination displaced; cancellation retained backup".into()),
            });
        }
        if let Err(error) = publish_no_replace(&output, destination) {
            let _ = cleanup_owned(&output);
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!("destination displaced; publish failed: {error:?}")),
            });
        }
        receipt.destination_published = true;
        #[cfg(test)]
        if fail_copy_replace_backup_cleanup_for_test(token, item) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("replacement published; backup cleanup failed: injected exact Copy Replace cleanup failure".into()),
            });
        }
        if let Err(error) = target.delete_backup() {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "replacement published; backup cleanup failed: {error:?}"
                )),
            });
        }
        receipt.displaced_backup_removed = true;
        // 置換先と退避ファイルの片付けを確認してから、公開に使った保持中ハンドルで証明を作る。
        if let Some(undo) = copy_undo_receipt_for_published_handle(
            destination,
            &output,
            &content_sha256,
            &target._ancestors,
        ) {
            on_copy_undo(undo);
        }
        Ok(ReplaceAttempt {
            receipt,
            detail: None,
        })
    }

    fn copy_undo_receipt_for_published_handle(
        destination: &Path,
        output: &std::fs::File,
        staged_sha256: &str,
        destination_ancestors: &[DirectoryLock],
    ) -> Option<crate::operations::model::CopyUndoReceipt> {
        let parent = destination_ancestors.first()?;
        let parent_identity = directory_identity_text(parent).ok()?;
        let identity = regular_file_identity(output).ok()?;
        let leaf_signature = leaf_signature(output).ok()?;
        let content_sha256 = hash_held_file(output).ok()?;
        if content_sha256 != staged_sha256
            || leaf_signature.number_of_links != 1
            || leaf_signature.change_time == 0
        {
            return None;
        }
        Some(crate::operations::model::CopyUndoReceipt {
            destination: destination.to_path_buf(),
            parent_identity,
            file_identity: format!(
                "{}:{:08x}{:08x}",
                identity.volume, identity.index_high, identity.index_low
            ),
            content_sha256,
            leaf_signature,
        })
    }

    fn same_copy_replace_manifest(
        entries: &[PinnedSourceEntry],
        children: &[super::super::journal::JournalCopyReplaceChild],
    ) -> bool {
        entries.len() == children.len()
            && entries.iter().all(|entry| {
                children.iter().any(|child| {
                    child.relative_components == entry.relative_components
                        && child.node_kind == entry.node_kind
                        && child.identity == entry.source_identity
                        && child.content_fact == entry.content_fact
                })
            })
    }

    fn capture_pinned_route(
        tree: &PinnedSourceTree,
        root: &Path,
    ) -> Result<Vec<(PathBuf, String)>, CopyError> {
        let mut paths = Vec::new();
        let mut cursor = root.parent();
        while let Some(path) = cursor {
            paths.push(path.to_path_buf());
            cursor = path.parent();
        }
        paths.reverse();
        if paths.len() != tree.root_handle_index {
            return Err(CopyError::Unsupported);
        }
        paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let Some(Some(PinnedSourceHandle::Directory(handle))) = tree._handles.get(index)
                else {
                    return Err(CopyError::Unsupported);
                };
                Ok((path, directory_identity_text(handle)?))
            })
            .collect()
    }

    pub(super) fn directory_move_replace_rename_candidate(
        source: &Path,
        destination: &Path,
        source_tree: &PinnedSourceTree,
        target_tree: &PinnedSourceTree,
    ) -> Result<super::super::model::DirectoryMoveReplaceRenameCandidate, CopyError> {
        use super::super::model::{
            DirectoryMoveReplaceRenameCandidate as Candidate,
            DirectoryMoveReplaceRenameCandidateChild as Child,
            DirectoryMoveReplaceRenameCandidateNodeKind as Kind,
        };
        let source_parent = source.parent().ok_or(CopyError::Unsupported)?;
        let destination_parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let source_route = capture_pinned_route(source_tree, source)?;
        let target_route = capture_pinned_route(target_tree, destination)?;
        let source_root = match source_tree._handles.get(source_tree.root_handle_index) {
            Some(Some(PinnedSourceHandle::Directory(root))) => root,
            _ => return Err(CopyError::Unsupported),
        };
        let target_root = match target_tree._handles.get(target_tree.root_handle_index) {
            Some(Some(PinnedSourceHandle::Directory(root))) => root,
            _ => return Err(CopyError::Unsupported),
        };
        let source_root_identity = directory_identity(source_root)?;
        let target_root_identity = directory_identity(target_root)?;
        if source_root_identity.volume != target_root_identity.volume {
            return Err(CopyError::Unsupported);
        }
        let source_parent_handle = source_tree
            .root_handle_index
            .checked_sub(1)
            .and_then(|index| match source_tree._handles.get(index) {
                Some(Some(PinnedSourceHandle::Directory(parent))) => Some(parent),
                _ => None,
            })
            .ok_or(CopyError::Unsupported)?;
        let destination_parent_handle = target_tree
            .root_handle_index
            .checked_sub(1)
            .and_then(|index| match target_tree._handles.get(index) {
                Some(Some(PinnedSourceHandle::Directory(parent))) => Some(parent),
                _ => None,
            })
            .ok_or(CopyError::Unsupported)?;
        let mut children = target_tree
            .entries
            .iter()
            .map(|entry| Ok(Child {
                ordinal: 0,
                node_kind: match entry.node_kind {
                    super::super::journal::JournalDirectoryNodeKind::File => Kind::File,
                    super::super::journal::JournalDirectoryNodeKind::Directory => Kind::Directory,
                    _ => return Err(CopyError::Unsupported),
                },
                relative_components: entry.relative_components.clone(),
                identity: entry.source_identity.clone(),
                content_fact: entry.content_fact.clone(),
            }))
            .collect::<Result<Vec<_>, CopyError>>()?;
        children.sort_by(|left, right| {
            right.relative_components.len().cmp(&left.relative_components.len())
                .then_with(|| {
                    let left = left.relative_components.iter().map(|part|
                        String::from_utf16_lossy(part).to_lowercase()).collect::<Vec<_>>();
                    let right = right.relative_components.iter().map(|part|
                        String::from_utf16_lossy(part).to_lowercase()).collect::<Vec<_>>();
                    left.cmp(&right)
                })
        });
        for (ordinal, child) in children.iter_mut().enumerate() {
            child.ordinal = u32::try_from(ordinal).map_err(|_| CopyError::Unsupported)?;
        }
        let source_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            source_root_identity.volume,
            (u64::from(source_root_identity.index_high) << 32)
                | u64::from(source_root_identity.index_low),
        );
        let target_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            target_root_identity.volume,
            (u64::from(target_root_identity.index_high) << 32)
                | u64::from(target_root_identity.index_low),
        );
        let source_parent_native = directory_identity(source_parent_handle)?;
        let destination_parent_native = directory_identity(destination_parent_handle)?;
        if source_parent_native.volume != source_root_identity.volume
            || destination_parent_native.volume != target_root_identity.volume
        {
            return Err(CopyError::Unsupported);
        }
        let source_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            source_parent_native.volume,
            (u64::from(source_parent_native.index_high) << 32)
                | u64::from(source_parent_native.index_low),
        );
        let destination_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            destination_parent_native.volume,
            (u64::from(destination_parent_native.index_high) << 32)
                | u64::from(destination_parent_native.index_low),
        );
        if source_route.last().map(|(_, identity)| identity) != Some(&directory_identity_text(source_parent_handle)?)
            || target_route.last().map(|(_, identity)| identity) != Some(&directory_identity_text(destination_parent_handle)?)
        {
            return Err(CopyError::Unsupported);
        }
        Ok(Candidate {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            source_identity,
            approved_target_identity: target_identity,
            volume_identity: source_root_identity.volume,
            source_parent: source_parent.to_path_buf(),
            source_parent_identity,
            destination_parent: destination_parent.to_path_buf(),
            destination_parent_identity,
            source_is_ordinary_directory: true,
            target_is_ordinary_directory: true,
            approved_target_children: children,
        })
    }

    pub(super) fn displace_approved_directory_move_replace_target(
        token: AttemptToken,
        conflict_token: super::super::model::ConflictToken,
        candidate: &super::super::model::DirectoryMoveReplaceRenameCandidate,
        mut source: PinnedSourceTree,
        mut target: PinnedSourceTree,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
        #[cfg(test)] mut test_hook: Option<DirectoryMoveReplaceRenameTestHook>,
    ) -> Result<super::super::journal::JournalDirectoryMoveReplaceRename, CopyError> {
        let mutation_may_have_happened = std::sync::atomic::AtomicBool::new(false);
        let result = (|| {
        use super::super::journal::{
            DirectoryMoveReplaceRenamePhase as Phase,
            DirectoryMoveReplaceSourceLocation as SourceLocation,
            DirectoryMoveReplaceTargetLocation as TargetLocation,
            JournalDirectoryMoveReplaceRename as Ledger,
            JournalDirectoryMoveReplaceRenameChild as Child,
            JournalPath,
        };
        let source_path = candidate.source.as_path();
        let destination = candidate.destination.as_path();
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        if cancelled.load(Ordering::Acquire)
            || candidate.source != candidate.source_parent.join(source_path.file_name().ok_or(CopyError::Unsupported)?)
            || candidate.destination != candidate.destination_parent.join(destination.file_name().ok_or(CopyError::Unsupported)?)
            || candidate.source_parent != source_path.parent().ok_or(CopyError::Unsupported)?
            || candidate.destination_parent != parent
        {
            return Err(CopyError::GuardRejected("approved directory Replace routes changed".into()));
        }
        let source_root = match source._handles.get(source.root_handle_index) {
            Some(Some(PinnedSourceHandle::Directory(root))) => root,
            _ => return Err(CopyError::Unsupported),
        };
        let target_root = match target._handles.get(target.root_handle_index) {
            Some(Some(PinnedSourceHandle::Directory(root))) => root,
            _ => return Err(CopyError::Unsupported),
        };
        let target_root_handle = target_root.0;
        let source_native = directory_identity(source_root)?;
        let target_native = directory_identity(target_root)?;
        let source_parent_native = directory_identity(match source._handles.get(source.root_handle_index - 1) {
            Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
            _ => return Err(CopyError::Unsupported),
        })?;
        let target_parent_native = directory_identity(match target._handles.get(target.root_handle_index - 1) {
            Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
            _ => return Err(CopyError::Unsupported),
        })?;
        let native_identity = |value: DirectoryIdentity| {
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                value.volume,
                (u64::from(value.index_high) << 32) | u64::from(value.index_low),
            )
        };
        let identity_text = |identity: &crate::filesystem_identity::FilesystemIdentity| {
            identity.windows_parts().map(|(volume, index)| format!("{volume}:{index:016x}"))
                .ok_or(CopyError::Unsupported)
        };
        if native_identity(source_native) != candidate.source_identity
            || native_identity(target_native) != candidate.approved_target_identity
            || native_identity(source_parent_native) != candidate.source_parent_identity
            || native_identity(target_parent_native) != candidate.destination_parent_identity
            || source_native.volume != target_native.volume
            || source_native.volume != candidate.volume_identity
            || source_parent_native.volume != source_native.volume
            || target_parent_native.volume != target_native.volume
            || source.root_identity != format!("{}:{:08x}{:08x}", source_native.volume, source_native.index_high, source_native.index_low)
            || target.root_identity != format!("{}:{:08x}{:08x}", target_native.volume, target_native.index_high, target_native.index_low)
            || target.entries.len() != candidate.approved_target_children.len()
        {
            return Err(CopyError::GuardRejected("approved directory Replace identity changed".into()));
        }
        for approved in &candidate.approved_target_children {
            if !target.entries.iter().any(|entry| {
                entry.relative_components == approved.relative_components
                    && entry.source_identity == approved.identity
                    && entry.content_fact == approved.content_fact
                    && entry.node_kind == match approved.node_kind {
                    super::super::model::DirectoryMoveReplaceRenameCandidateNodeKind::File => super::super::journal::JournalDirectoryNodeKind::File,
                    super::super::model::DirectoryMoveReplaceRenameCandidateNodeKind::Directory => super::super::journal::JournalDirectoryNodeKind::Directory,
                    }
            }) {
                return Err(CopyError::GuardRejected("approved target manifest changed".into()));
            }
        }
        let mut backup_suffix = [0u8; 16];
        getrandom::fill(&mut backup_suffix).map_err(|error| CopyError::Io(format!("generate directory Replace backup name: {error}")))?;
        let backup = parent.join(format!(".gpui-filer-move-replace-{}-{}-{:032x}", token.job_id().value(), token.attempt_id().value(), u128::from_be_bytes(backup_suffix)));
        if backup.exists() {
            return Err(CopyError::DestinationExists);
        }
        let mut children = candidate.approved_target_children.iter().map(|child| Child {
            ordinal: child.ordinal,
            node_kind: match child.node_kind {
                super::super::model::DirectoryMoveReplaceRenameCandidateNodeKind::File => super::super::journal::JournalDirectoryNodeKind::File,
                super::super::model::DirectoryMoveReplaceRenameCandidateNodeKind::Directory => super::super::journal::JournalDirectoryNodeKind::Directory,
            },
            relative_components: child.relative_components.clone(),
            identity: child.identity.clone(),
            content_fact: child.content_fact.clone(),
            disposition_recorded: false,
        }).collect::<Vec<_>>();
        if children.iter().any(|child| child.ordinal as usize >= children.len()) {
            return Err(CopyError::Unsupported);
        }
        let mut ledger = Ledger {
            item: 0,
            source: JournalPath::from_path(source_path),
            destination: JournalPath::from_path(destination),
            backup: JournalPath::from_path(&backup),
            source_identity: identity_text(&candidate.source_identity)?,
            approved_target_identity: identity_text(&candidate.approved_target_identity)?,
            volume_identity: format!("{:08x}", source_native.volume),
            source_parent: JournalPath::from_path(&candidate.source_parent),
            source_parent_identity: identity_text(&candidate.source_parent_identity)?,
            destination_parent: JournalPath::from_path(parent),
            destination_parent_identity: identity_text(&candidate.destination_parent_identity)?,
            phase: Phase::Prepared,
            source_location: SourceLocation::Original,
            target_location: TargetLocation::Destination,
            approved_target_children: std::mem::take(&mut children),
            pending_cleanup: None,
            backup_root_disposed: false,
        };
        // Keep the held ancestor routes live through the native rename.
        let _source_parent_route_identity = directory_identity_text(match source._handles.get(source.root_handle_index - 1) {
                Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
                _ => return Err(CopyError::Unsupported),
            })?;
        let _destination_parent_route_identity = directory_identity_text(match target._handles.get(target.root_handle_index - 1) {
                Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
                _ => return Err(CopyError::Unsupported),
            })?;
        wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)?;
        if cancelled.load(Ordering::Acquire) {
            ledger.phase = Phase::NoEffect;
            ledger.target_location = TargetLocation::Destination;
            wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)?;
            return Err(CopyError::Cancelled);
        }
        ledger.phase = Phase::DisplaceIntent;
        ledger.target_location = TargetLocation::Uncertain;
        match wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit) {
            Ok(()) => {}
            Err(DirectoryMoveReplaceRenamePhaseWaitError::ExactJournalRefusal) => {
                // A negative ACK proves this rename was never authorized to run.
                // Persist that no-effect fact before returning a normal failure.
                ledger.phase = Phase::NoEffect;
                ledger.source_location = SourceLocation::Original;
                ledger.target_location = TargetLocation::Destination;
                wait_for_directory_move_replace_rename_phase(
                    token,
                    conflict_token,
                    &ledger,
                    controls,
                    emit,
                )
                .map_err(|error| CopyError::Unknown(format!(
                    "DisplaceIntent was refused before mutation, but NoEffect acknowledgement failed: {error:?}"
                )))?;
                return Err(CopyError::DirectoryMoveReplaceNoEffect {
                    cancelled: cancelled.load(Ordering::Acquire),
                });
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref().and_then(|hook| hook.after_phase_ack.as_ref()) {
            hook(Phase::DisplaceIntent);
        }
        if cancelled.load(Ordering::Acquire) {
            ledger.phase = Phase::NoEffect;
            ledger.target_location = TargetLocation::Destination;
            wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
                .map_err(|error| CopyError::Unknown(format!("cancelled before displacement, but NoEffect acknowledgement failed: {error:?}")))?;
            return Err(CopyError::Cancelled);
        }
        if backup.exists() {
            ledger.phase = Phase::NoEffect;
            ledger.target_location = TargetLocation::Destination;
            wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
                .map_err(|error| CopyError::Unknown(format!("backup name was occupied before displacement, but NoEffect acknowledgement failed: {error:?}")))?;
            return Err(CopyError::DestinationExists);
        }
        let pre_displacement_check = (|| {
            if native_identity(directory_identity(source_root)?) != candidate.source_identity
                || native_identity(directory_identity(target_root)?) != candidate.approved_target_identity
                || native_identity(directory_identity(match source._handles.get(source.root_handle_index - 1) {
                    Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
                    _ => return Err(CopyError::Unsupported),
                })?) != candidate.source_parent_identity
                || native_identity(directory_identity(match target._handles.get(target.root_handle_index - 1) {
                    Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
                    _ => return Err(CopyError::Unsupported),
                })?) != candidate.destination_parent_identity
            {
                return Err(CopyError::GuardRejected("directory Replace pre-displacement check failed".into()));
            }
            Ok(())
        })();
        if let Err(error) = pre_displacement_check {
            ledger.phase = Phase::NoEffect;
            ledger.target_location = TargetLocation::Destination;
            if let Err(no_effect_error) = wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit) {
                return Err(CopyError::Unknown(format!("pre-displacement check failed ({error:?}); durable NoEffect acknowledgement failed ({no_effect_error:?})")));
            }
            return Err(error);
        }
        for entry in &target.entries {
            target._handles[entry.handle_index] = None;
        }
        let destination_parent_handle = match target._handles.get(target.root_handle_index - 1) {
            Some(Some(PinnedSourceHandle::Directory(parent))) => parent.0,
            _ => return Err(CopyError::Unsupported),
        };
        if directory_identity_text_handle(target_root_handle)? != ledger.approved_target_identity
            || directory_identity_text_handle(destination_parent_handle)?
                != ledger.destination_parent_identity
        {
            return Err(CopyError::GuardRejected(
                "approved target or destination parent changed before displacement".into(),
            ));
        }
        // The native rename may succeed even when its return path is interrupted.
        mutation_may_have_happened.store(true, Ordering::Release);
        let backup_leaf = backup.file_name().ok_or(CopyError::Unsupported)?;
        if let Err(error) = crate::operations::rename::rename_relative_handle_no_replace(
            target_root_handle.0,
            destination_parent_handle.0,
            backup_leaf,
        ) {
            return Err(CopyError::Unknown(format!("target displacement outcome is unresolved: {error:?}")));
        }
        ledger.phase = Phase::TargetDisplaced;
        ledger.target_location = TargetLocation::Backup;
        wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
            .map_err(|error| CopyError::Unknown(format!("target displaced; durable receipt unresolved: {error:?}")))?;
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref().and_then(|hook| hook.after_phase_ack.as_ref()) {
            hook(Phase::TargetDisplaced);
        }

        // Once displacement is durable, cancellation is partial: the old tree
        // remains held at backup and the incoming source stays at its origin.
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Partial(
                "target displaced; cancellation retained source and backup".into(),
            ));
        }

        ledger.phase = Phase::PublishIntent;
        ledger.source_location = SourceLocation::Uncertain;
        wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
            .map_err(|error| CopyError::Unknown(format!("target displaced; publish intent unresolved: {error:?}")))?;
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref().and_then(|hook| hook.after_phase_ack.as_ref()) {
            hook(Phase::PublishIntent);
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Partial(
                "target displaced; cancellation retained source and backup".into(),
            ));
        }

        let source_root_handle = source_root.0;
        if native_identity(directory_identity(source_root)?) != candidate.source_identity
            || native_identity(directory_identity(match target._handles.get(target.root_handle_index) {
                Some(Some(PinnedSourceHandle::Directory(root))) => root,
                _ => return Err(CopyError::Unsupported),
            })?) != candidate.approved_target_identity
            || native_identity(directory_identity(match source._handles.get(source.root_handle_index - 1) {
                Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
                _ => return Err(CopyError::Unsupported),
            })?) != candidate.source_parent_identity
            || native_identity(directory_identity(match target._handles.get(target.root_handle_index - 1) {
                Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
                _ => return Err(CopyError::Unsupported),
            })?) != candidate.destination_parent_identity
            || capture_pinned_route(&source, source_path).is_err()
            || capture_pinned_route(&target, destination).is_err()
        {
            return Err(CopyError::Partial(
                "target displaced; source publication identity or route check failed".into(),
            ));
        }
        match std::fs::symlink_metadata(destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(CopyError::Partial(
                    "target displaced; a foreign destination entry prevents publication".into(),
                ));
            }
            Err(error) => {
                return Err(CopyError::Partial(format!(
                    "target displaced; destination absence could not be verified: {error}"
                )));
            }
        }
        // Descendant handles deny delete sharing. Keep both roots and all
        // ancestor route handles alive while releasing only those descendants.
        for entry in &source.entries {
            source._handles[entry.handle_index] = None;
        }
        let destination_parent_handle = match target._handles.get(target.root_handle_index - 1) {
            Some(Some(PinnedSourceHandle::Directory(parent))) => parent.0,
            _ => return Err(CopyError::Unsupported),
        };
        #[cfg(test)]
        if let Some(hook) = test_hook.as_mut() {
            (hook.before_publish)(destination);
        }
        let publish_result = crate::operations::rename::rename_relative_handle_no_replace(
            source_root_handle.0,
            destination_parent_handle.0,
            destination.file_name().ok_or(CopyError::Unsupported)?,
        );
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref() {
            let _ = hook.publish_result.send(publish_result.clone());
        }
        if let Err(error) = publish_result {
            return Err(CopyError::Unknown(format!(
                "source publication outcome is unresolved; source and backup retained: {error:?}"
            )));
        }
        ledger.phase = Phase::DestinationPublished;
        ledger.source_location = SourceLocation::Published;
        ledger.target_location = TargetLocation::Backup;
        wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
            .map_err(|error| CopyError::Unknown(format!(
                "source published and backup retained; durable receipt unresolved: {error:?}"
            )))?;
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref().and_then(|hook| hook.after_phase_ack.as_ref()) {
            hook(Phase::DestinationPublished);
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Partial("source published; cancellation retained the approved backup".into()));
        }
        let backup_root = match target._handles.get(target.root_handle_index) {
            Some(Some(PinnedSourceHandle::Directory(root))) => root,
            _ => return Err(CopyError::Partial("published source and backup root custody was lost".into())),
        };
        let destination_parent = match target._handles.get(target.root_handle_index - 1) {
            Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
            _ => return Err(CopyError::Unsupported),
        };
        let source_parent = match source._handles.get(source.root_handle_index - 1) {
            Some(Some(PinnedSourceHandle::Directory(parent))) => parent,
            _ => return Err(CopyError::Unsupported),
        };
        if directory_identity_text(backup_root)? != ledger.approved_target_identity
            || directory_identity_text(destination_parent)? != ledger.destination_parent_identity
            || directory_identity_text(source_parent)? != ledger.source_parent_identity
            || directory_identity_text_handle(source_root_handle)? != ledger.source_identity
        {
            return Err(CopyError::Partial("published source, backup, or held parent identity changed before cleanup".into()));
        }
        let facts = ledger.approved_target_children.iter().map(|child| StagedChildFact {
            relative_components: child.relative_components.clone(), node_kind: child.node_kind,
            identity: child.identity.clone(), content_fact: child.content_fact.clone(),
        }).collect::<Vec<_>>();
        let mut reopened = reopen_staged_children_by_facts(backup_root, &facts)
            .map_err(|error| CopyError::Partial(format!("approved backup inventory could not be reacquired under its held root: {error:?}")))?;
        if reopened.len() != ledger.approved_target_children.len()
            || reopened.iter().any(|node| !ledger.approved_target_children.iter().any(|child|
                child.relative_components == node.relative_components && child.node_kind == node.node_kind
                    && child.identity == node.identity && child.content_fact == node.content_fact))
        {
            return Err(CopyError::Partial("approved backup inventory changed before cleanup".into()));
        }
        if cancelled.load(Ordering::Acquire)
            || open_pinned_directory_with_share(
                destination,
                false,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ).ok().is_none_or(|published| directory_identity_text(&published).ok().as_deref() != Some(&ledger.source_identity))
        {
            return Err(CopyError::Partial("published destination identity changed or cleanup was cancelled".into()));
        }
        ledger.phase = Phase::BackupCleanup;
        wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
            .map_err(|error| CopyError::Partial(format!("backup retained; BackupCleanup acknowledgement failed: {error:?}")))?;
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref().and_then(|hook| hook.after_phase_ack.as_ref()) {
            hook(Phase::BackupCleanup);
        }

        let cleanup_children = ledger.approved_target_children.clone();
        for child in &cleanup_children {
            if cancelled.load(Ordering::Acquire) { return Err(CopyError::Partial("backup cleanup cancelled; remaining backup retained".into())); }
            if directory_identity_text(backup_root)? != ledger.approved_target_identity
                || directory_identity_text(destination_parent)? != ledger.destination_parent_identity
                || directory_identity_text(source_parent)? != ledger.source_parent_identity
                || directory_identity_text_handle(source_root_handle)? != ledger.source_identity
                || open_pinned_directory_with_share(
                    destination,
                    false,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ).ok().is_none_or(|published| directory_identity_text(&published).ok().as_deref() != Some(&ledger.source_identity))
            { return Err(CopyError::Partial("held route or root identity changed during backup cleanup".into())); }
            let Some(node_index) = reopened.iter().position(|node| node.relative_components == child.relative_components) else { return Err(CopyError::Partial("approved backup child disappeared during cleanup".into())); };
            let node = &reopened[node_index];
            if node.identity != child.identity || node.node_kind != child.node_kind || node.content_fact != child.content_fact {
                return Err(CopyError::Partial("approved backup child identity changed during cleanup".into()));
            }
            let intent = super::super::journal::JournalDirectoryMoveReplaceRenameChildIntent {
                ordinal: child.ordinal, node_kind: child.node_kind, relative_components: child.relative_components.clone(),
                identity: child.identity.clone(), content_fact: child.content_fact.clone(),
            };
            ledger.pending_cleanup = Some(intent.clone());
            emit(ExecutorFact::DirectoryMoveReplaceRenameCleanupIntent { token, item: ItemId(0), intent: intent.clone() });
            match controls.recv() {
                Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupIntentJournaledExact { token: ack, item, intent: ack_intent })
                    if ack == token && item == ItemId(0) && ack_intent == intent && !cancelled.load(Ordering::Acquire) => {}
                _ => return Err(CopyError::Partial("backup child intent was not exactly acknowledged; pending intent retained".into())),
            }
            if directory_identity_text(backup_root)? != ledger.approved_target_identity || cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Partial("backup changed or cancellation arrived after durable child intent".into()));
            }
            set_handle_delete_disposition(&node.handle).map_err(|error| CopyError::Partial(format!("approved backup child disposition is uncertain: {error:?}")))?;
            emit(ExecutorFact::DirectoryMoveReplaceRenameCleanupDisposition { token, item: ItemId(0), intent: intent.clone() });
            match controls.recv() {
                Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupReceiptJournaledExact { token: ack, item, intent: ack_intent }) if ack == token && item == ItemId(0) && ack_intent == intent => {
                    drop(reopened.remove(node_index));
                    ledger.approved_target_children[child.ordinal as usize].disposition_recorded = true;
                    ledger.pending_cleanup = None;
                }
                _ => return Err(CopyError::Partial("backup child disposition may have happened; receipt ACK unresolved".into())),
            }
        }
        let root_intent = super::super::journal::JournalDirectoryMoveReplaceRenameChildIntent {
            ordinal: u32::try_from(ledger.approved_target_children.len()).map_err(|_| CopyError::Unsupported)?,
            node_kind: super::super::journal::JournalDirectoryNodeKind::Directory,
            relative_components: Vec::new(), identity: ledger.approved_target_identity.clone(), content_fact: "root".into(),
        };
        if cancelled.load(Ordering::Acquire) || !enumerate_directory_handle(backup_root, SOURCE_TREE_MAX_ENTRIES, SOURCE_TREE_MAX_NAME_BYTES)?.is_empty()
            || directory_identity_text(backup_root)? != ledger.approved_target_identity
        { return Err(CopyError::Partial("backup root is not empty and identity-pinned after child cleanup".into())); }
        ledger.pending_cleanup = Some(root_intent.clone());
        emit(ExecutorFact::DirectoryMoveReplaceRenameCleanupIntent { token, item: ItemId(0), intent: root_intent.clone() });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupIntentJournaledExact { token: ack, item, intent }) if ack == token && item == ItemId(0) && intent == root_intent && !cancelled.load(Ordering::Acquire) => {}
            _ => return Err(CopyError::Partial("backup root intent was not exactly acknowledged; pending intent retained".into())),
        }
        if cancelled.load(Ordering::Acquire) || !enumerate_directory_handle(backup_root, 1, 2)?.is_empty()
            || directory_identity_text(backup_root)? != ledger.approved_target_identity
        { return Err(CopyError::Partial("backup root changed after durable root intent".into())); }
        #[cfg(test)]
        if let Some(hook) = test_hook.as_ref().and_then(|hook| hook.before_root_disposition.as_ref()) {
            use std::os::windows::ffi::OsStringExt;
            let path = PathBuf::from(std::ffi::OsString::from_wide(&ledger.backup.units));
            hook(&path);
        }
        set_raw_handle_delete_disposition(backup_root.0).map_err(|error| CopyError::Partial(format!("empty backup root disposition is uncertain: {error:?}")))?;
        emit(ExecutorFact::DirectoryMoveReplaceRenameCleanupDisposition { token, item: ItemId(0), intent: root_intent.clone() });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplaceRenameCleanupReceiptJournaledExact { token: ack, item, intent }) if ack == token && item == ItemId(0) && intent == root_intent => {
                target._handles[target.root_handle_index] = None;
                ledger.backup_root_disposed = true;
                ledger.target_location = TargetLocation::Disposed;
                ledger.pending_cleanup = None;
            }
            _ => return Err(CopyError::Partial("backup root disposition may have happened; receipt ACK unresolved".into())),
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Partial("backup root was removed; cancellation stopped before BackupComplete".into()));
        }
        emit(ExecutorFact::DirectoryMoveReplaceRenameBackupComplete { token, item: ItemId(0) });
        match controls.recv() {
            Ok(WorkerControl::DirectoryMoveReplaceRenameBackupCompleteJournaledExact { token: ack, item }) if ack == token && item == ItemId(0) => {}
            _ => return Err(CopyError::Partial("BackupComplete was not durably acknowledged".into())),
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Partial("BackupComplete is durable; cancellation stopped before Complete".into()));
        }
        ledger.phase = Phase::BackupComplete;
        ledger.phase = Phase::Complete;
        wait_for_directory_move_replace_rename_phase(token, conflict_token, &ledger, controls, emit)
            .map_err(|error| CopyError::Partial(format!("backup removed but Complete ACK failed: {error:?}")))?;
        Ok(ledger)
        })();
        result.map_err(|error| {
            if !mutation_may_have_happened.load(Ordering::Acquire) {
                return error;
            }
            match error {
                other @ (CopyError::Partial(_) | CopyError::Unknown(_)) => other,
                other => CopyError::Partial(format!(
                    "directory Replace may have mutated the namespace; last durable ledger retained: {other:?}"
                )),
            }
        })
    }

    #[derive(Debug)]
    enum DirectoryMoveReplaceRenamePhaseWaitError {
        ExactJournalRefusal,
        LostOrMismatchedControl,
    }

    impl From<DirectoryMoveReplaceRenamePhaseWaitError> for CopyError {
        fn from(error: DirectoryMoveReplaceRenamePhaseWaitError) -> Self {
            CopyError::Partial(format!(
                "same-volume directory Replace journal acknowledgement failed: {error:?}"
            ))
        }
    }

    fn wait_for_directory_move_replace_rename_phase(
        token: AttemptToken,
        conflict_token: super::super::model::ConflictToken,
        ledger: &super::super::journal::JournalDirectoryMoveReplaceRename,
        controls: &Receiver<WorkerControl>,
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<(), DirectoryMoveReplaceRenamePhaseWaitError> {
        emit(ExecutorFact::DirectoryMoveReplaceRenamePhaseIntent {
            token,
            conflict_token,
            ledger: ledger.clone(),
        });
        loop {
            match controls.recv() {
                // Cancellation is also delivered on this channel. It changes the
                // shared flag immediately, but must not consume the exact durable
                // phase ACK this worker is waiting for.
                Ok(WorkerControl::Cancel) => continue,
                Ok(WorkerControl::DirectoryMoveReplaceRenamePhaseJournaledExact {
                    token: ack_token,
                    conflict_token: ack_conflict,
                    ledger: ack_ledger,
                }) if ack_token == token && ack_conflict == conflict_token && ack_ledger == *ledger => return Ok(()),
                Ok(WorkerControl::DirectoryMoveReplaceRenamePhaseJournalFailedExact {
                    token: ack_token,
                    conflict_token: ack_conflict,
                    ledger: ack_ledger,
                }) if ack_token == token && ack_conflict == conflict_token && ack_ledger == *ledger => return Err(DirectoryMoveReplaceRenamePhaseWaitError::ExactJournalRefusal),
                _ => return Err(DirectoryMoveReplaceRenamePhaseWaitError::LostOrMismatchedControl),
            }
        }
    }

    fn release_pinned_route(tree: &mut PinnedSourceTree) {
        for handle in tree._handles.iter_mut().take(tree.root_handle_index) {
            *handle = None;
        }
    }

    fn reopen_pinned_route(route: &[(PathBuf, String)]) -> Result<Vec<DirectoryLock>, CopyError> {
        route
            .iter()
            .map(|(path, expected)| {
                let handle = open_pinned_directory_with_share(
                    path,
                    false,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )
                .map_err(|error| {
                    CopyError::Io(format!("route open {}: {error:?}", path.display()))
                })?;
                if directory_identity_text(&handle)? != *expected {
                    return Err(CopyError::Unsupported);
                }
                Ok(handle)
            })
            .collect()
    }

    pub(super) fn cleanup_failed_copy_replace_stage(
        path: &Path,
        expected_identity: &str,
        original: CopyError,
    ) -> CopyError {
        let cleanup = (|| -> Result<(), CopyError> {
            if !path.exists() {
                return Ok(());
            }
            let root = open_pinned_directory(path, true)?;
            if directory_identity_text(&root)? != expected_identity {
                return Err(CopyError::Unsupported);
            }
            if !enumerate_directory_handle(&root, 1, 2)?.is_empty() {
                return Err(CopyError::Unsupported);
            }
            set_raw_handle_delete_disposition(root.0)?;
            drop(root);
            if path.exists() {
                return Err(CopyError::Unsupported);
            }
            Ok(())
        })();
        match cleanup {
            Ok(()) => original,
            Err(error) => staging_cleanup_partial(path, original, error),
        }
    }

    pub(super) fn replace_folder(
        source_path: &Path,
        mut source: PinnedSourceTree,
        mut target: PinnedSourceTree,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        on_bytes: &mut dyn FnMut(u64),
        emit: &mut impl FnMut(ExecutorFact),
    ) -> Result<ReplaceAttempt, CopyError> {
        if cancelled.load(Ordering::Acquire) || item.0 != 0 {
            return Err(CopyError::Cancelled);
        }
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let _root_name = destination.file_name().ok_or(CopyError::Unsupported)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| CopyError::Io(format!("generate Copy Replace identity: {error}")))?;
        let suffix = format!("{:032x}", u128::from_be_bytes(random));
        let stage_path = parent.join(format!(
            ".gpui-filer-copy-{}-{}-{suffix}",
            token.job_id().value(),
            token.attempt_id().value()
        ));
        let backup = parent.join(format!(
            ".gpui-filer-replace-{}-{}-{suffix}",
            token.job_id().value(),
            token.attempt_id().value()
        ));
        if stage_path.exists() || backup.exists() {
            return Err(CopyError::Unsupported);
        }

        let source_route = capture_pinned_route(&source, source_path)?;
        let target_route = capture_pinned_route(&target, destination)?;
        let target_parent_identity = target_route
            .last()
            .map(|(_, identity)| identity.clone())
            .ok_or(CopyError::Unsupported)?;
        release_pinned_route(&mut source);
        release_pinned_route(&mut target);
        let staging = create_attempt_staging_root(&stage_path)?;
        let stage_identity = staging.root_identity.clone();
        let staged =
            match stage_pinned_source_tree_with_progress(&mut source, staging, cancelled, |bytes| {
                on_bytes(bytes)
            }) {
                Ok(staged) => staged,
                Err(error) => {
                    return Err(cleanup_failed_copy_replace_stage(
                        &stage_path,
                        &stage_identity,
                        error,
                    ));
                }
            };
        macro_rules! before_displacement {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(error) => return Err(clean_staged_failure_error(staged, error)),
                }
            };
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(clean_staged_failure_error(staged, CopyError::Cancelled));
        }
        let target_root_handle = match target
            ._handles
            .get(target.root_handle_index)
            .and_then(Option::as_ref)
        {
            Some(PinnedSourceHandle::Directory(root)) => root.0,
            _ => return Err(clean_staged_failure_error(staged, CopyError::Unsupported)),
        };
        let source_route_pins = match reopen_pinned_route(&source_route) {
            Ok(pins) => pins,
            Err(error) => {
                return Err(clean_staged_failure_error(
                    staged,
                    CopyError::Io(format!("folder Replace reopen source route: {error:?}")),
                ));
            }
        };
        let target_route_pins = match reopen_pinned_route(&target_route) {
            Ok(pins) => pins,
            Err(error) => {
                return Err(clean_staged_failure_error(
                    staged,
                    CopyError::Io(format!("folder Replace reopen target route: {error:?}")),
                ));
            }
        };
        let target_parent_handle = match target_route_pins.last() {
            Some(parent) => parent.0,
            None => return Err(clean_staged_failure_error(staged, CopyError::Unsupported)),
        };
        if before_displacement!(directory_identity_text_handle(target_parent_handle))
            != target_parent_identity
        {
            return Err(clean_staged_failure_error(staged, CopyError::Unsupported));
        }
        if before_displacement!(directory_identity_text_handle(target_root_handle))
            != target.root_identity
        {
            return Err(clean_staged_failure_error(staged, CopyError::Unsupported));
        }
        let mut backup_suffix = [0u8; 16];
        if let Err(error) = getrandom::fill(&mut backup_suffix) {
            return Err(clean_staged_failure_error(
                staged,
                CopyError::Io(format!("generate Replace backup identity: {error}")),
            ));
        }
        let backup = parent.join(format!(
            ".gpui-filer-replace-{}-{}-{}",
            token.job_id().value(),
            token.attempt_id().value(),
            format!("{:032x}", u128::from_be_bytes(backup_suffix))
        ));
        if backup.exists() {
            return Err(clean_staged_failure_error(staged, CopyError::Unsupported));
        }
        emit(ExecutorFact::ReplaceBackupIntent {
            token,
            item,
            source: source_path.to_path_buf(),
            destination: destination.to_path_buf(),
            backup: backup.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::ReplaceBackupJournaledExact {
                token: ack_token,
                item: ack_item,
                source: ack_source,
                destination: ack_destination,
                backup: ack_backup,
            }) if ack_token == token
                && ack_item == item
                && ack_source == source_path
                && ack_destination == destination
                && ack_backup == backup => {}
            _ => return Err(clean_staged_failure_error(staged, CopyError::Cancelled)),
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(clean_staged_failure_error(staged, CopyError::Cancelled));
        }

        // Keep only the exact root and its ancestor route pinned for the rename.
        // Descendant handles deny FILE_SHARE_DELETE and would block this root move.
        for entry in &target.entries {
            target._handles[entry.handle_index] = None;
        }
        if before_displacement!(directory_identity_text_handle(target_root_handle))
            != target.root_identity
            || before_displacement!(directory_identity_text_handle(target_parent_handle))
                != target_parent_identity
            || cancelled.load(Ordering::Acquire)
        {
            return Err(clean_staged_failure_error(staged, CopyError::Unsupported));
        }
        if let Err(error) = publish_no_replace_handle(target_root_handle, &backup) {
            return Err(clean_staged_failure_error(
                staged,
                CopyError::Partial(format!(
                    "folder Replace root displacement failed: {error:?}"
                )),
            ));
        }
        target._handles[target.root_handle_index] = None;
        // After this point the backup is recovery data. Every failure returns a
        // factual Partial receipt and leaves unresolved bytes in place.
        let mut receipt = ReplaceReceipt {
            source: source_path.to_path_buf(),
            destination: destination.to_path_buf(),
            displaced_backup: Some(backup.clone()),
            destination_published: false,
            displaced_backup_removed: false,
        };
        let mut published = match publish_and_verify_staged_tree(staged, destination, cancelled) {
            Ok(published) => published,
            Err(PublishTreeFailure::BeforePublish(error)) => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "destination displaced; staged folder could not be published: {error:?}; backup retained"
                    )),
                });
            }
            Err(PublishTreeFailure::AfterPublish { error, tree }) => {
                receipt.destination_published = true;
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "destination was published but verification stopped at {}; {error:?}; displaced backup retained",
                        tree.root_path.display()
                    )),
                });
            }
        };
        receipt.destination_published = true;
        let published_parent_identity = match published.root_chain.last() {
            Some(parent) => match directory_identity_text(parent) {
                Ok(identity) => Some(identity),
                Err(error) => {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some(format!(
                            "published destination parent identity could not be read: {error:?}; backup retained"
                        )),
                    });
                }
            },
            None => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "published destination route handle is missing; backup retained".into(),
                    ),
                });
            }
        };
        if published_parent_identity.as_deref() != Some(&target_parent_identity) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "published destination parent differs from captured route; backup retained"
                        .into(),
                ),
            });
        }
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "destination published; cancellation retained displaced folder backup".into(),
                ),
            });
        }

        let mut children = target
            .entries
            .iter()
            .enumerate()
            .map(
                |(index, entry)| super::super::journal::JournalCopyReplaceChild {
                    ordinal: index as u32,
                    node_kind: entry.node_kind,
                    relative_components: entry.relative_components.clone(),
                    identity: entry.source_identity.clone(),
                    content_fact: entry.content_fact.clone(),
                    disposition_recorded: false,
                },
            )
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            right
                .relative_components
                .len()
                .cmp(&left.relative_components.len())
                .then_with(|| left.ordinal.cmp(&right.ordinal))
        });
        for (ordinal, child) in children.iter_mut().enumerate() {
            child.ordinal = ordinal as u32;
        }
        let ledger = super::super::journal::JournalCopyReplaceCleanup {
            item: item.0,
            destination: super::super::journal::JournalPath::from_path(destination),
            backup: super::super::journal::JournalPath::from_path(&backup),
            root_identity: target.root_identity.clone(),
            children,
            pending: None,
            root_disposition_recorded: false,
            cleanup_complete: false,
        };
        let manifest_size = match serde_json::to_vec(&ledger) {
            Ok(bytes) => bytes.len(),
            Err(error) => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "displaced folder manifest serialization failed: {error}; backup retained"
                    )),
                });
            }
        };
        if manifest_size > 3 * 1024 * 1024 {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "displaced folder manifest exceeds the durable journal bound; backup retained"
                        .into(),
                ),
            });
        }
        // The bounded scanner reopens the backup route itself; release only
        // these route handles for the scan, then pin and revalidate them again.
        // The staged lease's parent handle is write-capable and conflicts with
        // the scanner's read-only route open, so release that exact handle too.
        if published.root_chain.pop().is_none() {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "published destination parent handle is missing; backup retained".into(),
                ),
            });
        }
        drop(source_route_pins);
        drop(target_route_pins);
        let mut cleanup = match preflight_pinned_source_tree(&backup, cancelled) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "displaced folder could not be safely rescanned; backup retained: {error:?}"
                    )),
                });
            }
        };
        let _source_route_pins = match reopen_pinned_route(&source_route) {
            Ok(pins) => pins,
            Err(error) => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "source route changed after backup scan; backup retained: {error:?}"
                    )),
                });
            }
        };
        let target_route_pins = match reopen_pinned_route(&target_route) {
            Ok(pins) => pins,
            Err(error) => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "destination route changed after backup scan; backup retained: {error:?}"
                    )),
                });
            }
        };
        let target_parent_handle = match target_route_pins.last() {
            Some(parent) => parent.0,
            None => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some("destination parent route pin is missing; backup retained".into()),
                });
            }
        };
        if cleanup.root_identity != ledger.root_identity
            || !same_copy_replace_manifest(&cleanup.entries, &ledger.children)
        {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "displaced folder differs from its approved manifest; backup retained".into(),
                ),
            });
        }
        emit(ExecutorFact::CopyReplaceCleanupManifestIntent {
            token,
            ledger: ledger.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::CopyReplaceCleanupManifestJournaledExact { token: ack_token, ledger: ack_ledger }) if ack_token == token && ack_ledger == ledger && !cancelled.load(Ordering::Acquire) => {}
            _ => return Ok(ReplaceAttempt { receipt, detail: Some("displaced folder cleanup manifest was not durably acknowledged; backup retained".into()) }),
        }
        for child in &ledger.children {
            let cleanup_root = match cleanup
                ._handles
                .get(cleanup.root_handle_index)
                .and_then(Option::as_ref)
            {
                Some(PinnedSourceHandle::Directory(root)) => root,
                _ => {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some("backup root handle was lost; backup retained".into()),
                    });
                }
            };
            let source_root = match source
                ._handles
                .get(source.root_handle_index)
                .and_then(Option::as_ref)
            {
                Some(PinnedSourceHandle::Directory(root)) => root,
                _ => {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some("source root handle was lost; backup retained".into()),
                    });
                }
            };
            if cancelled.load(Ordering::Acquire) {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some("cleanup stopped: cancelled; backup retained".into()),
                });
            }
            if directory_identity_text(&published.root).ok().as_deref()
                != Some(&published.root_identity)
            {
                return Ok(ReplaceAttempt { receipt, detail: Some("cleanup stopped: published destination root identity mismatch; backup retained".into()) });
            }
            if published_parent_identity.as_deref() != Some(&target_parent_identity) {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "cleanup stopped: captured destination parent mismatch; backup retained"
                            .into(),
                    ),
                });
            }
            if directory_identity_text(cleanup_root).ok().as_deref() != Some(&ledger.root_identity)
            {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "cleanup stopped: backup root identity mismatch; backup retained".into(),
                    ),
                });
            }
            if directory_identity_text(source_root).ok().as_deref() != Some(&source.root_identity) {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "cleanup stopped: pinned source identity mismatch; backup retained".into(),
                    ),
                });
            }
            if directory_identity_text_handle(target_parent_handle)
                .ok()
                .as_deref()
                != Some(&target_parent_identity)
            {
                return Ok(ReplaceAttempt { receipt, detail: Some("cleanup stopped: pinned destination parent identity mismatch; backup retained".into()) });
            }
            let Some((_, entry)) = cleanup
                .entries
                .iter()
                .enumerate()
                .find(|(_, entry)| entry.relative_components == child.relative_components)
            else {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "displaced child is absent from its approved manifest; backup retained"
                            .into(),
                    ),
                });
            };
            if entry.node_kind != child.node_kind
                || entry.source_identity != child.identity
                || entry.content_fact != child.content_fact
            {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "displaced child identity or content changed; backup retained".into(),
                    ),
                });
            }
            let intent = super::super::journal::JournalCopyReplaceChildIntent {
                ordinal: child.ordinal,
                node_kind: child.node_kind,
                relative_components: child.relative_components.clone(),
                identity: child.identity.clone(),
                content_fact: child.content_fact.clone(),
            };
            emit(ExecutorFact::CopyReplaceCleanupChildIntent {
                token,
                item,
                intent: intent.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::CopyReplaceCleanupChildIntentJournaledExact {
                    token: ack_token,
                    item: ack_item,
                    intent: ack_intent,
                }) if ack_token == token
                    && ack_item == item
                    && ack_intent == intent
                    && !cancelled.load(Ordering::Acquire) => {}
                _ => {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some(
                            "cleanup intent was not durably acknowledged; backup retained".into(),
                        ),
                    });
                }
            }
            let Some(handle) = cleanup
                ._handles
                .get(entry.handle_index)
                .and_then(Option::as_ref)
            else {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some("approved backup child handle is missing; backup retained".into()),
                });
            };
            #[cfg(test)]
            // Shell が完全な子 intent の writer ACK を確認するまで native worker だけを止める。
            if fail_copy_replace_backup_child_cleanup_for_test(token, item, &intent) {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "cleanup disposition failed; backup retained: injected exact Copy Replace child disposition failure".into(),
                    ),
                });
            }
            if let Err(error) = set_handle_delete_disposition(handle) {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!(
                        "cleanup disposition failed; backup retained: {error:?}"
                    )),
                });
            }
            emit(ExecutorFact::CopyReplaceCleanupChildDisposition {
                token,
                item,
                intent: intent.clone(),
            });
            match controls.recv() {
                Ok(WorkerControl::CopyReplaceCleanupChildReceiptJournaledExact {
                    token: ack_token,
                    item: ack_item,
                    intent: ack_intent,
                }) if ack_token == token && ack_item == item && ack_intent == intent => {
                    cleanup._handles[entry.handle_index] = None
                }
                _ => {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some(
                            "cleanup receipt was not durably acknowledged; backup retained".into(),
                        ),
                    });
                }
            }
        }
        let root_intent = super::super::journal::JournalCopyReplaceChildIntent {
            ordinal: ledger.children.len() as u32,
            node_kind: super::super::journal::JournalDirectoryNodeKind::Directory,
            relative_components: Vec::new(),
            identity: ledger.root_identity.clone(),
            content_fact: "root".into(),
        };
        emit(ExecutorFact::CopyReplaceCleanupChildIntent {
            token,
            item,
            intent: root_intent.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::CopyReplaceCleanupChildIntentJournaledExact {
                token: ack_token,
                item: ack_item,
                intent,
            }) if ack_token == token
                && ack_item == item
                && intent == root_intent
                && !cancelled.load(Ordering::Acquire) => {}
            _ => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some("backup-root intent was not acknowledged; backup retained".into()),
                });
            }
        }
        let Some(PinnedSourceHandle::Directory(cleanup_root)) = cleanup
            ._handles
            .get(cleanup.root_handle_index)
            .and_then(Option::as_ref)
        else {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("backup root handle was lost; backup retained".into()),
            });
        };
        let cleanup_root_identity = match directory_identity_text(cleanup_root) {
            Ok(identity) => identity,
            Err(error) => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(format!("backup root identity query failed: {error:?}; backup retained")),
                });
            }
        };
        if cleanup_root_identity != ledger.root_identity {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("backup root identity changed; backup retained".into()),
            });
        }
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("backup root disposition cancelled; backup retained".into()),
            });
        }
        if let Err(error) = set_raw_handle_delete_disposition(cleanup_root.0) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!("backup root native delete disposition failed: {error:?}; backup retained")),
            });
        }
        emit(ExecutorFact::CopyReplaceCleanupChildDisposition {
            token,
            item,
            intent: root_intent.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::CopyReplaceCleanupChildReceiptJournaledExact {
                token: ack_token,
                item: ack_item,
                intent,
            }) if ack_token == token && ack_item == item && intent == root_intent => {
                cleanup._handles[cleanup.root_handle_index] = None
            }
            _ => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "backup-root receipt was not acknowledged; backup retained".into(),
                    ),
                });
            }
        }
        emit(ExecutorFact::CopyReplaceCleanupComplete { token, item });
        match controls.recv() {
            Ok(WorkerControl::CopyReplaceCleanupCompleteJournaledExact {
                token: ack_token,
                item: ack_item,
            }) if ack_token == token && ack_item == item => {}
            _ => {
                return Ok(ReplaceAttempt {
                    receipt,
                    detail: Some(
                        "backup cleanup completion was not durable; backup retained".into(),
                    ),
                });
            }
        }
        receipt.displaced_backup_removed = !backup.exists();
        if receipt.displaced_backup_removed {
            Ok(ReplaceAttempt {
                receipt,
                detail: None,
            })
        } else {
            Ok(ReplaceAttempt {
                receipt,
                detail: Some("backup root remains after acknowledged cleanup".into()),
            })
        }
    }

    pub(super) fn replace_move_regular_file(
        source: &Path,
        source_handle: ReplaceSource,
        destination: &Path,
        target: ReplaceTarget,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut dyn FnMut(ExecutorFact),
    ) -> Result<ReplaceAttempt, CopyError> {
        replace_move_regular_file_with_final_hash_reader(
            source,
            source_handle,
            destination,
            target,
            expected_parent,
            token,
            item,
            cancelled,
            controls,
            emit,
            |_, _| None,
        )
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum FinalHashFile {
        Source,
        Stage,
    }

    pub(super) fn replace_move_regular_file_with_final_hash_reader(
        source: &Path,
        source_handle: ReplaceSource,
        destination: &Path,
        target: ReplaceTarget,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut dyn FnMut(ExecutorFact),
        mut final_hash_failure: impl FnMut(AttemptToken, FinalHashFile) -> Option<std::io::Error>,
    ) -> Result<ReplaceAttempt, CopyError> {
        let source_identity = regular_file_identity(&source_handle.file)?;
        let target_identity = target.identity()?;
        if source_identity == target_identity {
            return Err(CopyError::Unsupported);
        }
        if source_identity.volume != target_identity.volume {
            return replace_cross_volume_move_regular_file(
                source,
                source_handle,
                destination,
                target,
                expected_parent,
                token,
                item,
                source_identity,
                target_identity,
                &mut final_hash_failure,
                cancelled,
                controls,
                emit,
            );
        }
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let mut backup_suffix = [0u8; 16];
        getrandom::fill(&mut backup_suffix).map_err(|error| {
            CopyError::Io(format!("generate Move Replace backup identity: {error}"))
        })?;
        let backup = parent.join(format!(
            ".gpui-filer-replace-{}-{}-{:032x}",
            token.job_id().value(),
            token.attempt_id().value(),
            u128::from_be_bytes(backup_suffix)
        ));
        emit(ExecutorFact::ReplaceBackupIntent {
            token,
            item,
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            backup: backup.clone(),
        });
        match controls.recv() {
            Ok(WorkerControl::ReplaceBackupJournaledExact {
                token: actual_token,
                item: actual_item,
                source: actual_source,
                destination: actual_destination,
                backup: actual_backup,
            }) if actual_token == token
                && actual_item == item
                && actual_source == source
                && actual_destination == destination
                && actual_backup == backup => {}
            Ok(WorkerControl::Cancel) | Err(_) => return Err(CopyError::Cancelled),
            Ok(_) => return Err(CopyError::Unsupported),
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        if regular_file_identity(&source_handle.file)? != source_identity
            || target.identity()? != target_identity
        {
            return Err(CopyError::Unsupported);
        }
        let mut receipt = ReplaceReceipt {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            displaced_backup: Some(backup.clone()),
            destination_published: false,
            displaced_backup_removed: false,
        };
        if let Err(error) = target.displace_to(&backup) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "target displacement could not be established; inspect source, destination, and backup: {error:?}"
                )),
            });
        }
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "destination displaced; cancellation retained source and backup".into(),
                ),
            });
        }
        if let Err(error) = crate::operations::rename::rename_held_file_no_replace(
            &source_handle.file,
            destination,
            expected_parent,
        ) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "destination displaced; held source publish failed: {error:?}"
                )),
            });
        }
        receipt.destination_published = true;
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("source published; cancellation retained backup".into()),
            });
        }
        if let Err(error) = target.delete_backup() {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "source published; backup cleanup failed: {error:?}"
                )),
            });
        }
        receipt.displaced_backup_removed = true;
        // Move Replace の Undo 証明は公開済みの保持ハンドルと、事前に保持した親 route から作る。
        // 退避対象の復元権限は含めず、証明が取れなくても成功した置換自体は維持する。
        if !cancelled.load(Ordering::Acquire)
            && let (Some(source_parent), Some(destination_parent)) =
                (source.parent(), destination.parent())
        {
            let source_paths: Vec<_> = source_parent.ancestors().collect();
            let destination_paths: Vec<_> = destination_parent.ancestors().collect();
            let source_route = source_handle._ancestors.iter().rev();
            let destination_route = target._ancestors.iter().rev();
            let source_parent_route = source_paths
                .iter()
                .rev()
                .zip(source_route)
                .map(|(path, handle)| {
                    let identity = directory_identity(handle)?;
                    Ok(crate::operations::model::SourceCleanupParentIdentity {
                        path: (*path).to_path_buf(),
                        identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                            identity.volume,
                            (u64::from(identity.index_high) << 32)
                                | u64::from(identity.index_low),
                        ),
                    })
                })
                .collect::<Result<Vec<_>, CopyError>>();
            let destination_parent_route = destination_paths
                .iter()
                .rev()
                .zip(destination_route)
                .map(|(path, handle)| {
                    let identity = directory_identity(handle)?;
                    Ok(crate::operations::model::SourceCleanupParentIdentity {
                        path: (*path).to_path_buf(),
                        identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                            identity.volume,
                            (u64::from(identity.index_high) << 32)
                                | u64::from(identity.index_low),
                        ),
                    })
                })
                .collect::<Result<Vec<_>, CopyError>>();
            if let (Ok(source_parent_route), Ok(destination_parent_route)) =
                (source_parent_route, destination_parent_route)
                && source_parent_route.last().is_some_and(|parent| parent.path == source_parent)
                && destination_parent_route
                    .last()
                    .is_some_and(|parent| parent.path == destination_parent)
                && source_parent_route
                    .last()
                    .zip(destination_parent_route.last())
                    .is_some_and(|(source, destination)| {
                        source.identity.windows_parts().map(|(volume, _)| volume)
                            == destination.identity.windows_parts().map(|(volume, _)| volume)
                            && source.identity.windows_parts().map(|(volume, _)| volume)
                                == Some(source_identity.volume)
                    })
                && regular_file_identity(&source_handle.file).is_ok_and(|published| {
                    published == source_identity
                        && published.volume == target_identity.volume
                })
            {
                emit(ExecutorFact::MoveUndoCandidate {
                    token,
                    item,
                    receipt: crate::operations::model::MoveUndoReceipt {
                        source: source.to_path_buf(),
                        destination: destination.to_path_buf(),
                        object_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                            source_identity.volume,
                            (u64::from(source_identity.index_high) << 32)
                                | u64::from(source_identity.index_low),
                        ),
                        object_kind: crate::operations::model::MoveUndoObjectKind::RegularFile,
                        source_parent_route,
                        destination_parent_route,
                        cross_volume_file: None,
                    },
                });
            }
        }
        Ok(ReplaceAttempt {
            receipt,
            detail: None,
        })
    }

    fn replace_cross_volume_move_regular_file(
        source: &Path,
        source_handle: ReplaceSource,
        destination: &Path,
        target: ReplaceTarget,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
        token: AttemptToken,
        item: ItemId,
        source_identity: RegularFileIdentity,
        target_identity: RegularFileIdentity,
        final_hash_failure: &mut impl FnMut(AttemptToken, FinalHashFile) -> Option<std::io::Error>,
        cancelled: &AtomicBool,
        controls: &Receiver<WorkerControl>,
        emit: &mut dyn FnMut(ExecutorFact),
    ) -> Result<ReplaceAttempt, CopyError> {
        use sha2::Digest;
        use std::io::{Read, Seek, SeekFrom, Write};

        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        ensure_local_fixed_ntfs(source)?;
        ensure_local_fixed_ntfs(destination)?;
        let destination_parent = target._ancestors.first().ok_or(CopyError::Unsupported)?;
        let destination_parent_identity = directory_identity(destination_parent)?;
        let parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            destination_parent_identity.volume,
            (u64::from(destination_parent_identity.index_high) << 32)
                | u64::from(destination_parent_identity.index_low),
        );
        if parent_identity != *expected_parent {
            return Err(CopyError::Unsupported);
        }
        let source_path = crate::operations::journal::JournalPath::from_path(source);
        let destination_journal_path =
            crate::operations::journal::JournalPath::from_path(destination);
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| {
            CopyError::Io(format!("generate Move Replace stage identity: {error}"))
        })?;
        let tag = format!(
            "{}-{}-{:032x}",
            token.job_id().value(),
            token.attempt_id().value(),
            u128::from_be_bytes(random)
        );
        let stage = parent.join(format!(".gpui-filer-move-stage-{tag}"));
        let backup = parent.join(format!(".gpui-filer-replace-{}", tag));
        let mut base = crate::operations::journal::JournalCrossVolumeMoveReplace {
            item: item.0,
            source: source_path,
            stage: crate::operations::journal::JournalPath::from_path(&stage),
            destination: destination_journal_path,
            backup: crate::operations::journal::JournalPath::from_path(&backup),
            source_identity: identity_text(source_identity.shared())?,
            stage_identity: String::new(),
            approved_target_identity: identity_text(target_identity.shared())?,
            destination_parent_identity: identity_text(parent_identity)?,
            content_sha256: String::new(),
            phase: crate::operations::journal::CrossVolumeMoveReplacePhase::StageCreateIntent,
        };
        cross_volume_replace_phase(token, base.clone(), controls, emit)?;

        if cancelled.load(Ordering::Acquire) {
            let mut closed = base.clone();
            closed.phase = crate::operations::journal::CrossVolumeMoveReplacePhase::StageNotCreated;
            cross_volume_replace_phase(token, closed, controls, emit)
                .map_err(|error| CopyError::Partial(format!("cancelled before stage creation but StageNotCreated was not durably acknowledged: {error:?}")))?;
            return Err(CopyError::Cancelled);
        }

        let mut output = match create_protected_move_replace_stage(&stage) {
            Ok(output) => output,
            Err(error) => {
                let mut closed = base.clone();
                closed.phase =
                    crate::operations::journal::CrossVolumeMoveReplacePhase::StageNotCreated;
                cross_volume_replace_phase(token, closed, controls, emit)
                    .map_err(|journal_error| CopyError::Partial(format!("CREATE_NEW failed and StageNotCreated was not durably acknowledged: {journal_error:?}; create error: {error}")))?;
                return Err(io(error));
            }
        };
        let stage_identity = match regular_file_identity(&output) {
            Ok(identity) => identity,
            Err(error) => {
                cleanup_owned(&output).map_err(|cleanup| CopyError::Partial(format!("created stage identity could not be read and retained-handle cleanup failed: {cleanup:?}; {error:?}")))?;
                return Err(CopyError::Partial(format!(
                    "created stage identity could not be read; retained handle was disposed: {error:?}"
                )));
            }
        };
        base.stage_identity = identity_text(stage_identity.shared())?;
        base.phase = crate::operations::journal::CrossVolumeMoveReplacePhase::StageOwned;
        if let Err(error) = cross_volume_replace_phase(token, base.clone(), controls, emit) {
            cleanup_owned(&output).map_err(|cleanup| CopyError::Partial(format!("StageOwned acknowledgement failed and exact retained-handle cleanup failed: {cleanup:?}; {error:?}")))?;
            drop(output);
            let mut discarded = base.clone();
            discarded.phase =
                crate::operations::journal::CrossVolumeMoveReplacePhase::StageDiscarded;
            cross_volume_replace_phase(token, discarded, controls, emit).map_err(|receipt_error| CopyError::Partial(format!("StageOwned acknowledgement failed; exact retained-handle cleanup completed but StageDiscarded was not durably acknowledged: {receipt_error:?}; original acknowledgement: {error:?}")))?;
            return Err(error);
        }
        if stage_identity.volume != destination_parent_identity.volume {
            cleanup_owned(&output).map_err(|cleanup| CopyError::Partial(format!("created stage identity has the wrong volume and retained-handle cleanup failed: {cleanup:?}")))?;
            drop(output);
            let mut discarded = base.clone();
            discarded.phase =
                crate::operations::journal::CrossVolumeMoveReplacePhase::StageDiscarded;
            cross_volume_replace_phase(token, discarded, controls, emit).map_err(|journal_error| CopyError::Partial(format!("wrong-volume stage cleanup completed but StageDiscarded was not durably acknowledged: {journal_error:?}")))?;
            return Err(CopyError::Unsupported);
        }
        let staged = (|| -> Result<(String, RegularFileIdentity), CopyError> {
            let mut input = source_handle.file.try_clone().map_err(io)?;
            input.seek(SeekFrom::Start(0)).map_err(io)?;
            let mut buffer = vec![0; CHUNK];
            let mut copied_hash = sha2::Sha256::new();
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return Err(CopyError::Cancelled);
                }
                let count = input.read(&mut buffer).map_err(io)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count]).map_err(io)?;
                copied_hash.update(&buffer[..count]);
                #[cfg(all(test, target_os = "windows"))]
                super::tests::checkpoint_cross_volume_replace_after_partial_stage_write(
                    &base, count,
                );
                #[cfg(all(test, target_os = "windows"))]
                if super::tests::take_cross_volume_replace_partial_stage_write_failure_for_test(&base) {
                    return Err(CopyError::Io(
                        "injected cross-volume Replace failure after a successful stage chunk".into(),
                    ));
                }
            }
            #[cfg(all(test, target_os = "windows"))]
            super::tests::checkpoint_cross_volume_replace_after_full_stage_write(&base);
            #[cfg(all(test, target_os = "windows"))]
            if super::tests::take_cross_volume_replace_stage_flush_failure_for_test(&base) {
                return Err(CopyError::Io("injected cross-volume Replace stage flush failure".into()));
            }
            flush_staged_file(&output)?;
            let content_sha256 = format!("{:x}", copied_hash.finalize());
            if hash_held_file(&source_handle.file)? != content_sha256
                || hash_held_file(&output)? != content_sha256
            {
                return Err(CopyError::Unsupported);
            }
            #[cfg(all(test, target_os = "windows"))]
            super::tests::checkpoint_cross_volume_replace_after_flushed_stage_write(
                &base,
                &content_sha256,
            );
            Ok((content_sha256, stage_identity))
        })();
        let (content_sha256, stage_identity) = match staged {
            Ok(staged) => staged,
            Err(error) => {
                cleanup_owned(&output).map_err(|cleanup_error| {
                    CopyError::Partial(format!(
                        "pre-displacement stage cleanup failed at {}: {cleanup_error:?}",
                        stage.display()
                    ))
                })?;
                drop(output);
                let mut discarded = base.clone();
                discarded.phase =
                    crate::operations::journal::CrossVolumeMoveReplacePhase::StageDiscarded;
                cross_volume_replace_phase(token, discarded, controls, emit).map_err(|journal_error| CopyError::Partial(format!("stage cleanup completed but StageDiscarded was not durably acknowledged: {journal_error:?}; original staging failure: {error:?}")))?;
                return Err(error);
            }
        };
        base.content_sha256 = content_sha256;
        base.phase = crate::operations::journal::CrossVolumeMoveReplacePhase::StageReady;
        if let Err(error) = cross_volume_replace_phase(token, base.clone(), controls, emit) {
            cleanup_owned(&output).map_err(|cleanup| CopyError::Partial(format!("phase acknowledgement failed before displacement; stage cleanup uncertain: {cleanup:?}; {error:?}")))?;
            drop(output);
            let mut discarded = base.clone();
            discarded.phase =
                crate::operations::journal::CrossVolumeMoveReplacePhase::StageDiscarded;
            cross_volume_replace_phase(token, discarded, controls, emit).map_err(|journal_error| CopyError::Partial(format!("stage cleanup completed after StageReady acknowledgement failure but StageDiscarded was not durably acknowledged: {journal_error:?}; original acknowledgement: {error:?}")))?;
            return Err(error);
        }
        let mut receipt = ReplaceReceipt {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            displaced_backup: Some(backup.clone()),
            destination_published: false,
            displaced_backup_removed: false,
        };
        macro_rules! phase {
            ($phase:ident) => {{
                let mut next = base.clone();
                next.phase = crate::operations::journal::CrossVolumeMoveReplacePhase::$phase;
                if let Err(error) =
                    cross_volume_replace_phase(token, next.clone(), controls, emit)
                {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some(format!(
                            "cross-volume Move Replace phase {:?} not durably acknowledged; inspect source, destination, stage, and backup: {error:?}",
                            crate::operations::journal::CrossVolumeMoveReplacePhase::$phase
                        )),
                    });
                }
                base = next;
            }};
            ($phase:ident, terminal) => {{
                // 終端相の後は次のledger遷移がないため、ACK用の値だけを送る。
                let mut next = base.clone();
                next.phase = crate::operations::journal::CrossVolumeMoveReplacePhase::$phase;
                if let Err(error) = cross_volume_replace_phase(token, next, controls, emit) {
                    return Ok(ReplaceAttempt {
                        receipt,
                        detail: Some(format!(
                            "cross-volume Move Replace phase {:?} not durably acknowledged; inspect source, destination, stage, and backup: {error:?}",
                            crate::operations::journal::CrossVolumeMoveReplacePhase::$phase
                        )),
                    });
                }
            }};
        }
        if cancelled.load(Ordering::Acquire) {
            cleanup_owned(&output).map_err(|error| {
                CopyError::Partial(format!(
                    "cancelled before displacement; stage cleanup uncertain: {error:?}"
                ))
            })?;
            phase!(StageDiscarded, terminal);
            return Err(CopyError::Cancelled);
        }
        phase!(DisplaceIntent);
        if cancelled.load(Ordering::Acquire) {
            if let Err(error) = cleanup_owned(&output) {
                return Ok(ReplaceAttempt {
                    receipt: ReplaceReceipt {
                        source: source.to_path_buf(),
                        destination: destination.to_path_buf(),
                        displaced_backup: Some(backup.clone()),
                        destination_published: false,
                        displaced_backup_removed: false,
                    },
                    detail: Some(format!(
                        "cancelled before displacement; stage cleanup uncertain: {error:?}"
                    )),
                });
            }
            phase!(StageDiscarded, terminal);
            return Err(CopyError::Cancelled);
        }
        if let Err(error) = target.displace_to(&backup) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "target displacement result is ambiguous; retained source, stage, destination, and backup for inspection: {error:?}"
                )),
            });
        }
        phase!(TargetDisplaced);
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "destination displaced; cancellation retained source, stage, and backup".into(),
                ),
            });
        }
        phase!(PublishIntent);
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("target displaced; cancellation retained source, stage, destination, and backup before publish".into()),
            });
        }
        if let Err(error) = publish_no_replace(&output, destination) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "destination displaced; stage publish failed: {error:?}"
                )),
            });
        }
        receipt.destination_published = true;
        phase!(DestinationPublished);
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(
                    "destination published; cancellation retained source and backup".into(),
                ),
            });
        }
        phase!(SourceDeleteIntent);
        let disposition = verify_then_dispose(
            cancelled,
            || {
                if regular_file_identity(&source_handle.file)? != source_identity
                    || regular_file_identity(&output)? != stage_identity
                    || final_hash_file(
                        final_hash_failure,
                        token,
                        FinalHashFile::Source,
                        &source_handle.file,
                    )? != base.content_sha256
                    || final_hash_file(final_hash_failure, token, FinalHashFile::Stage, &output)?
                        != base.content_sha256
                    || target_identity.volume == source_identity.volume
                {
                    return Err(CopyError::Unsupported);
                }
                Ok(())
            },
            || {
                #[cfg(test)]
                if tests::fail_cross_volume_source_disposition_after_ack(&base) {
                    return Err(CopyError::Io("injected exact source disposition failure".into()));
                }
                delete_exact_source(&source_handle.file)
            },
        );
        if let Err(error) = disposition {
            return Ok(post_publish_guard_failure(receipt, error));
        }
        phase!(SourceDeleted);
        phase!(BackupDeleteIntent);
        if cancelled.load(Ordering::Acquire) {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some("source deleted; cancellation retained the exact held backup".into()),
            });
        }
        if let Err(error) = target.delete_backup() {
            return Ok(ReplaceAttempt {
                receipt,
                detail: Some(format!(
                    "source deleted; exact held backup cleanup failed: {error:?}"
                )),
            });
        }
        receipt.displaced_backup_removed = true;
        phase!(BackupDeleted);
        phase!(Complete, terminal);
        Ok(ReplaceAttempt {
            receipt,
            detail: None,
        })
    }

    fn cross_volume_replace_phase(
        token: AttemptToken,
        ledger: crate::operations::journal::JournalCrossVolumeMoveReplace,
        controls: &Receiver<WorkerControl>,
        emit: &mut dyn FnMut(ExecutorFact),
    ) -> Result<(), CopyError> {
        emit(ExecutorFact::CrossVolumeMoveReplacePhaseIntent {
            token,
            ledger: ledger.clone(),
        });
        super::wait_for_cross_volume_move_replace_phase_ack(token, &ledger, controls)
    }

    fn hash_held_file(file: &std::fs::File) -> Result<String, CopyError> {
        use std::io::Read;
        hash_held_file_with_read(file, |handle, buffer| handle.read(buffer))
    }

    fn hash_held_file_with_read(
        file: &std::fs::File,
        mut read_chunk: impl FnMut(&mut std::fs::File, &mut [u8]) -> std::io::Result<usize>,
    ) -> Result<String, CopyError> {
        use sha2::Digest;
        use std::io::{Seek, SeekFrom};
        let mut handle = file.try_clone().map_err(io)?;
        handle.seek(SeekFrom::Start(0)).map_err(io)?;
        let mut hash = sha2::Sha256::new();
        let mut buffer = vec![0; CHUNK];
        loop {
            let count = read_chunk(&mut handle, &mut buffer).map_err(io)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    fn final_hash_file(
        inject_failure: &mut impl FnMut(AttemptToken, FinalHashFile) -> Option<std::io::Error>,
        token: AttemptToken,
        file_kind: FinalHashFile,
        file: &std::fs::File,
    ) -> Result<String, CopyError> {
        use std::io::Read;
        let mut injected_error = inject_failure(token, file_kind);
        hash_held_file_with_read(file, |handle, buffer| match injected_error.take() {
            Some(error) => Err(error),
            None => handle.read(buffer),
        })
    }

    pub(super) fn verify_then_dispose(
        cancelled: &AtomicBool,
        verify: impl FnOnce() -> Result<(), CopyError>,
        dispose: impl FnOnce() -> Result<(), CopyError>,
    ) -> Result<(), CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        verify()?;
        // Hashing may take a long time. Recheck immediately before source disposition.
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        dispose()
    }

    pub(super) fn partial_replace_attempt(
        receipt: ReplaceReceipt,
        detail: String,
    ) -> ReplaceAttempt {
        ReplaceAttempt {
            receipt,
            detail: Some(detail),
        }
    }

    pub(super) fn post_publish_guard_failure(
        receipt: ReplaceReceipt,
        error: CopyError,
    ) -> ReplaceAttempt {
        partial_replace_attempt(
            receipt,
            format!(
                "destination published; source/stage guard or disposition failed, all remaining paths retained: {error:?}"
            ),
        )
    }

    pub(super) fn create_protected_move_replace_stage(
        path: &Path,
    ) -> std::io::Result<std::fs::File> {
        use ::windows::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ},
        };
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_READ.0 | GENERIC_WRITE.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
    }

    #[cfg(test)]
    pub(super) fn write_mapping_is_rejected(file: &std::fs::File) -> bool {
        use ::windows::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::Memory::{CreateFileMappingW, PAGE_READWRITE},
        };
        use std::os::windows::io::AsRawHandle;
        let mapping = unsafe {
            CreateFileMappingW(
                HANDLE(file.as_raw_handle() as *mut _),
                None,
                PAGE_READWRITE,
                0,
                0,
                ::windows::core::PCWSTR::null(),
            )
        };
        match mapping {
            Ok(handle) => {
                let _ = unsafe { CloseHandle(handle) };
                false
            }
            Err(_) => true,
        }
    }

    #[cfg(test)]
    pub(super) fn source_write_mapping_is_rejected(source: &ReplaceSource) -> bool {
        write_mapping_is_rejected(&source.file)
    }

    #[cfg(test)]
    pub(super) fn later_write_mapping_is_blocked(path: &Path) -> bool {
        use ::windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use std::os::windows::fs::OpenOptionsExt;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(path)
        {
            Ok(file) => file,
            Err(_) => return true,
        };
        write_mapping_is_rejected(&file)
    }

    fn identity_text(
        identity: crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<String, CopyError> {
        let (volume, index) = identity.windows_parts().ok_or(CopyError::Unsupported)?;
        Ok(format!("{volume:08x}:{index:016x}"))
    }

    pub(super) fn copy(
        source: &Path,
        destination: &Path,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
    ) -> Result<(), CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        if source.is_dir()
            || source
                .symlink_metadata()
                .map_err(io)?
                .file_type()
                .is_symlink()
        {
            return Err(CopyError::Unsupported);
        }
        let input = std::fs::File::open(source).map_err(io)?;
        copy_open_regular_file(&input, destination, cancelled, on_bytes, None).map(|_| ())
    }

    pub(super) fn copy_regular_file_with_undo(
        source: &Path,
        destination: &Path,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
    ) -> Result<crate::operations::model::CopyUndoReceipt, CopyError> {
        copy_regular_file_with_undo_observed(
            source,
            destination,
            cancelled,
            crate::operations::model::ItemId(0),
            on_bytes,
            &mut |_| {},
            None,
        )
    }

    pub(super) fn copy_regular_file_with_undo_observed(
        source: &Path,
        destination: &Path,
        cancelled: &AtomicBool,
        item: crate::operations::model::ItemId,
        on_bytes: &mut dyn FnMut(u64),
        on_failure: &mut dyn FnMut(crate::operations::model::CopyFailureProvenance),
        retry_guard: Option<&crate::operations::model::CopyRetryGuard>,
    ) -> Result<crate::operations::model::CopyUndoReceipt, CopyError> {
        copy_regular_file_with_undo_observed_and_custody(
            None,
            source,
            destination,
            cancelled,
            item,
            on_bytes,
            on_failure,
            &mut |_| {},
            retry_guard,
        )
    }

    fn report_copy_retry_preflight_failure(
        retry_guard: Option<&crate::operations::model::CopyRetryGuard>,
        item: crate::operations::model::ItemId,
        source: &Path,
        destination: &Path,
        on_failure: &mut dyn FnMut(crate::operations::model::CopyFailureProvenance),
    ) {
        if retry_guard.is_some() {
            // 再inspectionは起動するが、openできていないpathのidentityを観測済みとは記録しない。
            on_failure(crate::operations::model::CopyFailureProvenance {
                item,
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
                publication: crate::operations::model::CopyPublicationState::NotPublished,
                source_identity: None,
                source_signature: None,
                source_signature_semantics:
                    crate::operations::model::CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
                destination_parent_identity: None,
                stage: None,
            });
        }
    }

    pub(super) fn copy_regular_file_with_undo_observed_and_custody(
        token: Option<AttemptToken>,
        source: &Path,
        destination: &Path,
        cancelled: &AtomicBool,
        item: crate::operations::model::ItemId,
        on_bytes: &mut dyn FnMut(u64),
        on_failure: &mut dyn FnMut(crate::operations::model::CopyFailureProvenance),
        on_stage_custody: &mut dyn FnMut(crate::operations::runtime::CopyStageCustody),
        retry_guard: Option<&crate::operations::model::CopyRetryGuard>,
    ) -> Result<crate::operations::model::CopyUndoReceipt, CopyError> {
        let source_path = source.to_path_buf();
        let source = match prepare_regular_file_source(source) {
            Ok(source) => source,
            Err(error) => {
                report_copy_retry_preflight_failure(
                    retry_guard,
                    item,
                    &source_path,
                    destination,
                    on_failure,
                );
                return Err(error);
            }
        };
        let Some(parent_path) = destination.parent() else {
            report_copy_retry_preflight_failure(
                retry_guard,
                item,
                &source_path,
                destination,
                on_failure,
            );
            return Err(CopyError::Unsupported);
        };
        let ancestors = match lock_non_reparse_ancestors(parent_path) {
            Ok(ancestors) => ancestors,
            Err(error) => {
                report_copy_retry_preflight_failure(
                    retry_guard,
                    item,
                    &source_path,
                    destination,
                    on_failure,
                );
                return Err(error);
            }
        };
        let Some(parent) = ancestors.first() else {
            report_copy_retry_preflight_failure(
                retry_guard,
                item,
                &source_path,
                destination,
                on_failure,
            );
            return Err(CopyError::Unsupported);
        };
        let parent_identity = match directory_identity_text(parent) {
            Ok(identity) => identity,
            Err(error) => {
                report_copy_retry_preflight_failure(
                    retry_guard,
                    item,
                    &source_path,
                    destination,
                    on_failure,
                );
                return Err(error);
            }
        };
        let source_identity = source.shared_identity();
        let source_signature = leaf_signature(&source.file).ok();
        let parent_observation = directory_identity(&parent).ok().map(|identity| {
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                identity.volume,
                (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
            )
        });
        let make_provenance =
            |publication, stage: Option<crate::operations::model::CopyStageObservation>| {
                crate::operations::model::CopyFailureProvenance {
                    item,
                    source: source_path.clone(),
                    destination: destination.to_path_buf(),
                    publication,
                    source_identity: source_identity.clone(),
                    source_signature,
                    source_signature_semantics: crate::operations::model::CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
                    destination_parent_identity: parent_observation.clone(),
                    stage,
                }
            };
        if let Some(guard) = retry_guard {
            if guard.item != item
                || guard.source != source_path
                || guard.destination != destination
                || source_identity.as_ref() != Some(&guard.source_identity)
                || source_signature != Some(guard.source_signature)
                || parent_observation.as_ref() != Some(&guard.destination_parent_identity)
            {
                on_failure(make_provenance(
                    crate::operations::model::CopyPublicationState::NotPublished,
                    None,
                ));
                return Err(CopyError::Io(
                    "Copy retry source or destination parent changed after inspection".into(),
                ));
            }
            match std::fs::symlink_metadata(destination) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => {
                    on_failure(make_provenance(
                        crate::operations::model::CopyPublicationState::NotPublished,
                        None,
                    ));
                    return Err(CopyError::DestinationExists);
                }
                Err(error) => {
                    on_failure(make_provenance(
                        crate::operations::model::CopyPublicationState::NotPublished,
                        None,
                    ));
                    return Err(CopyError::Io(error.to_string()));
                }
            }
        }
        let mut failure_provenance_emitted = false;
        let staged = stage_open_regular_file(
            &source.file,
            destination,
            cancelled,
            on_bytes,
            true,
            &mut |path, file| {
                let stage_identity = regular_file_identity(&file)
                    .ok()
                    .map(|identity| identity.shared());
                let stage = crate::operations::model::CopyStageObservation {
                    path: path.to_path_buf(),
                    identity: stage_identity.clone(),
                };
                failure_provenance_emitted = true;
                on_failure(make_provenance(
                    crate::operations::model::CopyPublicationState::NotPublished,
                    Some(stage),
                ));
                if let (Some(identity), Some(parent_identity)) =
                    (stage_identity, parent_observation.clone())
                {
                    if let Some(token) = token {
                        on_stage_custody(crate::operations::runtime::CopyStageCustody {
                            token,
                            item,
                            path: path.to_path_buf(),
                            identity,
                            parent_identity,
                            handle: file,
                        });
                    }
                }
            },
        );
        let (output, content_sha256, stage_path) = match staged {
            Ok(staged) => staged,
            Err(error) => {
                if !failure_provenance_emitted
                    && !matches!(&error, CopyError::Cancelled)
                    && source_identity.is_some()
                    && source_signature.is_some()
                    && parent_observation.is_some()
                {
                    on_failure(make_provenance(
                        crate::operations::model::CopyPublicationState::NotPublished,
                        None,
                    ));
                }
                return Err(error);
            }
        };
        if let Err(error) = publish_no_replace(&output, destination) {
            let retained_stage_identity = regular_file_identity(&output)
                .ok()
                .map(|identity| identity.shared());
            let error = cleanup_stage_error(output, &stage_path, error, &mut |path, file| {
                if let (Some(token), Some(identity), Some(parent_identity)) = (
                    token,
                    retained_stage_identity.clone(),
                    parent_observation.clone(),
                ) {
                    on_stage_custody(crate::operations::runtime::CopyStageCustody {
                        token,
                        item,
                        path: path.to_path_buf(),
                        identity,
                        parent_identity,
                        handle: file,
                    });
                }
            });
            if matches!(error, CopyError::PrePublishPartial(_)) {
                let stage = crate::operations::model::CopyStageObservation {
                    path: stage_path.clone(),
                    identity: retained_stage_identity,
                };
                on_failure(make_provenance(
                    crate::operations::model::CopyPublicationState::Unknown,
                    Some(stage),
                ));
            } else if retry_guard.is_some() {
                on_failure(make_provenance(
                    crate::operations::model::CopyPublicationState::NotPublished,
                    None,
                ));
            }
            return Err(error);
        }
        let stage_observation = || crate::operations::model::CopyStageObservation {
            path: stage_path.clone(),
            identity: regular_file_identity(&output)
                .ok()
                .map(|identity| identity.shared()),
        };
        #[cfg(test)]
        if COPY_TEST_FAIL_RECEIPT_FACTS.with(|value| value.replace(false)) {
            on_failure(make_provenance(
                crate::operations::model::CopyPublicationState::Published,
                Some(stage_observation()),
            ));
            return Err(CopyError::Unknown(format!(
                "destination published at {}; injected receipt fact failure",
                destination.display()
            )));
        }
        let identity = regular_file_identity(&output).map_err(|error| {
            on_failure(make_provenance(
                crate::operations::model::CopyPublicationState::Published,
                Some(stage_observation()),
            ));
            CopyError::Unknown(format!(
                "destination published at {}; receipt identity failed: {error:?}",
                destination.display()
            ))
        })?;
        let leaf_signature = leaf_signature(&output).map_err(|error| {
            on_failure(make_provenance(
                crate::operations::model::CopyPublicationState::Published,
                Some(stage_observation()),
            ));
            CopyError::Unknown(format!(
                "destination published at {}; receipt leaf facts failed: {error:?}",
                destination.display()
            ))
        })?;
        let parent_after = directory_identity_text(parent).map_err(|error| {
            on_failure(make_provenance(
                crate::operations::model::CopyPublicationState::Published,
                Some(stage_observation()),
            ));
            CopyError::Unknown(format!(
                "destination published at {}; destination parent validation failed: {error:?}",
                destination.display()
            ))
        })?;
        if parent_after != parent_identity {
            on_failure(make_provenance(
                crate::operations::model::CopyPublicationState::Published,
                Some(stage_observation()),
            ));
            return Err(CopyError::Unknown(format!(
                "destination published at {}; destination parent identity changed",
                destination.display()
            )));
        }
        Ok(crate::operations::model::CopyUndoReceipt {
            destination: destination.to_path_buf(),
            parent_identity,
            file_identity: format!(
                "{}:{:08x}{:08x}",
                identity.volume, identity.index_high, identity.index_low
            ),
            content_sha256,
            leaf_signature,
        })
    }

    pub(super) fn undo_copy(
        receipt: &crate::operations::model::CopyUndoReceipt,
    ) -> Result<(), CopyError> {
        undo_exact_ordinary_file(
            &receipt.destination,
            &receipt.parent_identity,
            &receipt.file_identity,
            &receipt.content_sha256,
            receipt.leaf_signature,
            None,
        )
    }

    fn undo_exact_ordinary_file(
        destination: &Path,
        expected_parent_identity: &str,
        expected_file_identity: &str,
        expected_content_sha256: &str,
        expected_signature: crate::operations::model::SourceCleanupLeafSignature,
        expected_size: Option<u64>,
    ) -> Result<(), CopyError> {
        use sha2::Digest;
        use std::io::Read;

        let parent_path = destination.parent().ok_or(CopyError::Unsupported)?;
        let ancestors = lock_non_reparse_ancestors(parent_path)?;
        let parent = ancestors.first().ok_or(CopyError::Unsupported)?;
        if directory_identity_text(parent)? != expected_parent_identity {
            return Err(CopyError::Unsupported);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(destination)
            .map_err(io)?;
        let identity = regular_file_identity(&file)?;
        let identity_text = format!(
            "{}:{:08x}{:08x}",
            identity.volume, identity.index_high, identity.index_low
        );
        if identity_text != expected_file_identity {
            return Err(CopyError::Unsupported);
        }
        if leaf_signature(&file)? != expected_signature {
            return Err(CopyError::Unsupported);
        }
        if expected_size.is_some_and(|size| file.metadata().map(|metadata| metadata.len()).ok() != Some(size)) {
            return Err(CopyError::Unsupported);
        }
        let mut hasher = sha2::Sha256::new();
        let mut reader = file.try_clone().map_err(io)?;
        let mut buffer = vec![0; CHUNK];
        loop {
            let count = reader.read(&mut buffer).map_err(io)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        if format!("{:x}", hasher.finalize()) != expected_content_sha256 {
            return Err(CopyError::Unsupported);
        }
        if directory_identity_text(parent)? != expected_parent_identity {
            return Err(CopyError::Unsupported);
        }
        delete_exact_source(&file)
    }

    pub(super) fn undo_created_object(
        receipt: &crate::operations::model::CreatedObjectUndoReceipt,
    ) -> Result<(), CopyError> {
        match &receipt.kind {
            crate::operations::model::CreatedObjectUndoKind::EmptyFile { content_sha256, signature } => {
                undo_exact_ordinary_file(
                    &receipt.destination,
                    &receipt.parent_identity,
                    &receipt.object_identity,
                    content_sha256,
                    *signature,
                    None,
                )
            }
            crate::operations::model::CreatedObjectUndoKind::ShortcutFile { content_sha256, signature, size } => {
                undo_exact_ordinary_file(
                    &receipt.destination,
                    &receipt.parent_identity,
                    &receipt.object_identity,
                    content_sha256,
                    *signature,
                    Some(*size),
                )
            }
            crate::operations::model::CreatedObjectUndoKind::EmptyFolder => {
                use std::os::windows::ffi::OsStrExt;

                let parent_path = receipt.destination.parent().ok_or(CopyError::Unsupported)?;
                let route = lock_virtual_destination(parent_path, None)?;
                let parent = route.last().ok_or(CopyError::Unsupported)?;
                if directory_identity_text(parent)? != receipt.parent_identity {
                    return Err(CopyError::Unsupported);
                }
                let name = receipt.destination.file_name().ok_or(CopyError::Unsupported)?;
                let component = name.encode_wide().collect::<Vec<_>>();
                let handle = nt_open_or_create_child(
                    parent,
                    &component,
                    true,
                    true,
                    NT_FILE_OPEN,
                    FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
                    false,
                )?;
                let directory = DirectoryLock(handle);
                let identity = directory_identity(&directory)?;
                let identity_text = format!("{}:{:08x}{:08x}", identity.volume, identity.index_high, identity.index_low);
                if identity_text != receipt.object_identity
                    || !enumerate_directory_handle(&directory, 1, 2)?.is_empty()
                    || directory_identity_text(parent)? != receipt.parent_identity
                {
                    return Err(CopyError::Unsupported);
                }
                set_raw_handle_delete_disposition(directory.0)
            }
        }
    }

    /// The source handle is kept open without FILE_SHARE_WRITE/DELETE from
    /// before copy until after the source deletion. This makes the destructive
    /// half of a cross-volume move identity-bound rather than path-bound.
    pub(super) fn move_cross_volume_regular_file(
        source: &Path,
        destination: &Path,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        after_publish: &mut dyn FnMut(),
        item: crate::operations::model::ItemId,
        retry_guard: Option<&crate::operations::model::CrossVolumeMoveRetryGuard>,
        expected_source_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
        expected_destination_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
        on_source_retained: &mut dyn FnMut(crate::operations::model::PublishedMoveSourceRetainedProof),
        on_disposition: &mut dyn FnMut(crate::operations::model::PublishedMoveDispositionProof),
        on_no_effect: &mut dyn FnMut(crate::operations::model::CrossVolumeMoveNoEffectProof),
    ) -> Result<(), CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let source_path = source.to_path_buf();
        let destination_path = destination.to_path_buf();
        let source_parent = source.parent().ok_or(CopyError::Unsupported)?;
        let destination_parent = destination.parent().ok_or(CopyError::Unsupported)?;
        // Keep root-first no-follow routes pinned through source disposition.
        // Undo proof must bind the original parent before its source leaf is deleted.
        let mut source_route_paths = Vec::new();
        let source_route_handles = lock_destination_route_root_first(source_parent, |path| {
            source_route_paths.push(path.to_path_buf());
        })?;
        let mut destination_route_paths = Vec::new();
        let destination_route_handles = lock_destination_route_root_first(destination_parent, |path| {
            destination_route_paths.push(path.to_path_buf());
        })?;
        let source_parent_route = parent_route_proof(&source_route_paths, &source_route_handles)?;
        let destination_parent_route = parent_route_proof(&destination_route_paths, &destination_route_handles)?;
        let destination_parent_handle = destination_route_handles.last().ok_or(CopyError::Unsupported)?;
        let source = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(source)
            .map_err(io)?;
        let source_identity = regular_file_identity(&source)?;
        if expected_source_identity.is_some_and(|expected| *expected != source_identity.shared()) {
            return Err(CopyError::GuardRejected(
                "clipboard Move source identity changed before cross-volume mutation".into(),
            ));
        }
        let source_signature = leaf_signature(&source)?;
        let destination_parent_identity = directory_identity(destination_parent_handle)?;
        if expected_destination_identity.is_some_and(|expected| {
            expected != &crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                destination_parent_identity.volume,
                (u64::from(destination_parent_identity.index_high) << 32)
                    | u64::from(destination_parent_identity.index_low),
            )
        }) {
            return Err(CopyError::GuardRejected(
                "clipboard Move destination identity changed before cross-volume publication".into(),
            ));
        }
        if source_identity.volume == destination_parent_identity.volume {
            return Err(CopyError::Unsupported);
        }
        if let Some(guard) = retry_guard {
            let proof = &guard.proof;
            if proof.item != item
                || proof.source != source_path
                || proof.destination != destination_path
                || proof.source_identity != source_identity.shared()
                || proof.source_signature != source_signature
                || !cross_volume_retry_guard_has_reliable_signature(guard)
                || proof.destination_parent_identity
                    != crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                        destination_parent_identity.volume,
                        (u64::from(destination_parent_identity.index_high) << 32)
                            | u64::from(destination_parent_identity.index_low),
                    )
                || !matches!(std::fs::symlink_metadata(destination), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                return Err(CopyError::Unsupported);
            }
        }
        // Keep the published output open without FILE_SHARE_DELETE until the
        // source disposition has committed. Otherwise another process could
        // replace the destination between publish and source deletion.
        // no-effect証明を伴う検査用失敗を、実際のcross-volume Move adapterで一度だけ起こす。
        #[cfg(test)]
        let destination_handle_result = if take_cross_volume_move_stage_create_failure_for_test() {
            Err(CopyError::Io("injected cross-volume Move stage creation failure".into()))
        } else {
            copy_open_regular_file(
                &source,
                destination,
                cancelled,
                on_bytes,
                Some(destination_parent_identity),
            )
        };
        #[cfg(not(test))]
        let destination_handle_result = copy_open_regular_file(
            &source,
            destination,
            cancelled,
            on_bytes,
            Some(destination_parent_identity),
        );
        let destination_handle = match destination_handle_result {
            Ok(handle) => handle,
            Err(error) => {
                // Publication ambiguity and retained-stage uncertainty must not
                // become retry evidence. A clean, non-Partial failure is paired
                // with unchanged held-handle facts and a no-follow leaf lookup.
                if !matches!(error, CopyError::Partial(_))
                    && !cancelled.load(Ordering::Acquire)
                    && source_signature.is_reliable_for_cross_volume_move_retry()
                    && matches!(std::fs::symlink_metadata(&destination), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)
                    && regular_file_identity(&source).is_ok_and(|id| id == source_identity)
                    && leaf_signature(&source).is_ok_and(|signature| signature == source_signature)
                    && directory_identity(destination_parent_handle).is_ok_and(|id| id == destination_parent_identity)
                {
                    on_no_effect(crate::operations::model::CrossVolumeMoveNoEffectProof {
                        item,
                        source: source_path,
                        destination: destination_path,
                        source_identity: source_identity.shared(),
                        source_signature,
                        destination_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                            destination_parent_identity.volume,
                            (u64::from(destination_parent_identity.index_high) << 32) | u64::from(destination_parent_identity.index_low),
                        ),
                    });
                }
                return Err(error);
            }
        };
        after_publish();
        // Hash is optional Undo evidence. A hashing failure must not erase the
        // ordinary publication/disposition fact or turn a successful Move into failure.
        let undo_content_sha256 = hash_held_file(&destination_handle).ok();

        let mut report_source_retained = || {
            if let (Ok(destination_identity), Ok(destination_signature), Ok(source_signature)) = (
                regular_file_identity(&destination_handle),
                leaf_signature(&destination_handle),
                leaf_signature(&source),
            ) {
                if let Ok(parent_identity) = directory_identity(destination_parent_handle) {
                    on_source_retained(
                        crate::operations::model::PublishedMoveSourceRetainedProof {
                            item,
                            source: source_path.clone(),
                            destination: destination_path.clone(),
                            source_identity: source_identity.shared(),
                            source_signature,
                            destination_parent_identity:
                                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                                    parent_identity.volume,
                                    (u64::from(parent_identity.index_high) << 32)
                                        | u64::from(parent_identity.index_low),
                                ),
                            destination_identity: destination_identity.shared(),
                            destination_signature,
                        },
                    );
                }
            }
        };

        // Once publish has happened, cancellation means source retention, not
        // rollback. A fresh retry would otherwise duplicate a visible file.
        if cancelled.load(Ordering::Acquire) {
            report_source_retained();
            return Err(CopyError::Partial(
                "destination published; cancellation retained source".into(),
            ));
        }
        match regular_file_identity(&source) {
            Ok(identity) if identity == source_identity => {}
            Ok(_) => {
                report_source_retained();
                return Err(CopyError::Partial(
                    "destination published; source identity changed before deletion".into(),
                ));
            }
            Err(error) => {
                report_source_retained();
                return Err(CopyError::Partial(format!(
                    "destination published; source revalidation failed: {error:?}"
                )));
            }
        }
        match leaf_signature(&source) {
            Ok(signature) if signature == source_signature => {}
            Ok(_) => {
                report_source_retained();
                return Err(CopyError::Partial(
                    "destination published; source signature changed before deletion".into(),
                ));
            }
            Err(error) => {
                report_source_retained();
                return Err(CopyError::Partial(format!(
                    "destination published; source signature revalidation failed: {error:?}"
                )));
            }
        }
        #[cfg(test)]
        let source_disposition = if take_cross_volume_move_source_disposition_failure_for_test(item, &source_path) {
            Err(CopyError::Io("injected exact direct Move source disposition failure".into()))
        } else {
            delete_exact_source(&source)
        };
        #[cfg(not(test))]
        let source_disposition = delete_exact_source(&source);
        if let Err(error) = source_disposition {
            report_source_retained();
            return Err(CopyError::Partial(format!(
                "destination published; source deletion failed: {error:?}"
            )));
        }
        if let (Ok(destination_identity), Ok(destination_signature), Ok(parent_identity)) = (
            regular_file_identity(&destination_handle), leaf_signature(&destination_handle),
            directory_identity(destination_parent_handle),
        ) {
            on_disposition(crate::operations::model::PublishedMoveDispositionProof {
                item, source: source_path, destination: destination_path,
                source_identity: source_identity.shared(), source_signature,
                source_pre_disposition_signature: source_signature,
                destination_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                    parent_identity.volume, (u64::from(parent_identity.index_high) << 32) | u64::from(parent_identity.index_low)),
                destination_identity: destination_identity.shared(), destination_signature,
                source_parent_route, destination_parent_route, content_sha256: undo_content_sha256,
            });
        }
        Ok(())
    }

    fn copy_open_regular_file(
        source: &std::fs::File,
        destination: &Path,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        expected_parent: Option<DirectoryIdentity>,
    ) -> Result<std::fs::File, CopyError> {
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let lock = lock_directory(parent)?;
        if let Some(expected) = expected_parent {
            if directory_identity(&lock)? != expected {
                return Err(CopyError::Unsupported);
            }
        }
        let _held_parent = lock;
        copy_open_regular_file_in_parent(source, destination, cancelled, on_bytes, false)
    }

    fn copy_open_regular_file_in_parent(
        source: &std::fs::File,
        destination: &Path,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        strict_custody: bool,
    ) -> Result<std::fs::File, CopyError> {
        let (output, _, stage_path) = stage_open_regular_file(
            source,
            destination,
            cancelled,
            on_bytes,
            strict_custody,
            &mut |_, _| {},
        )?;
        if let Err(error) = publish_no_replace(&output, destination) {
            return Err(cleanup_stage_error(
                output,
                &stage_path,
                error,
                &mut |_, _| {},
            ));
        }
        Ok(output)
    }

    /// Produces an owned, synced temporary file in the destination parent but
    /// does not make the requested name visible yet. Replace needs this gap so
    /// it can durably record and displace the old object first.
    fn stage_open_regular_file(
        source: &std::fs::File,
        destination: &Path,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        strict_custody: bool,
        on_cleanup_failure: &mut dyn FnMut(&Path, std::fs::File),
    ) -> Result<(std::fs::File, String, PathBuf), CopyError> {
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let temp = parent.join(format!(
            ".gpui-filer-copy-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut input = source.try_clone().map_err(io)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE.0 | GENERIC_READ.0 | DELETE.0)
            .share_mode(if strict_custody {
                // Cleanup authority depends on the original handle pinning the
                // stage against external writable and delete-capable opens.
                FILE_SHARE_READ.0
            } else {
                FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0
            })
            .open(&temp)
            .map_err(io)?;
        let mut buffer = vec![0; CHUNK];
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(cleanup_stage_error(
                    output,
                    &temp,
                    CopyError::Cancelled,
                    on_cleanup_failure,
                ));
            }
            let count = match input.read(&mut buffer) {
                Ok(count) => count,
                Err(error) => {
                    return Err(cleanup_stage_error(
                        output,
                        &temp,
                        io(error),
                        on_cleanup_failure,
                    ));
                }
            };
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            if let Err(error) = output.write_all(&buffer[..count]) {
                return Err(cleanup_stage_error(
                    output,
                    &temp,
                    io(error),
                    on_cleanup_failure,
                ));
            }
            on_bytes(count as u64);
            // progress を出した後だけ検証用 delay を置く。EOF / sync / publish の後へ
            // 遅延を持ち込まないので、通常の durable step や Replace barrier は変わらない。
            if let Err(error) = wait_for_copy_throttle(cancelled) {
                return Err(cleanup_stage_error(
                    output,
                    &temp,
                    error,
                    on_cleanup_failure,
                ));
            }
        }
        if let Err(error) = output.sync_all() {
            return Err(cleanup_stage_error(
                output,
                &temp,
                io(error),
                on_cleanup_failure,
            ));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(cleanup_stage_error(
                output,
                &temp,
                CopyError::Cancelled,
                on_cleanup_failure,
            ));
        }
        Ok((output, format!("{:x}", hasher.finalize()), temp))
    }

    fn cleanup_stage_error(
        file: std::fs::File,
        path: &Path,
        original: CopyError,
        on_cleanup_failure: &mut dyn FnMut(&Path, std::fs::File),
    ) -> CopyError {
        match cleanup_owned(&file) {
            Ok(()) => original,
            Err(cleanup) => {
                on_cleanup_failure(path, file);
                CopyError::PrePublishPartial(format!(
                    "pre-publication stage cleanup failed at {} (original operation error: {original:?}; cleanup error: {cleanup:?})",
                    path.display()
                ))
            }
        }
    }

    pub(super) fn copy_entry(
        source: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
        on_directory_undo: &mut dyn FnMut(super::super::model::DirectoryCopyUndoReceipt),
    ) -> Result<(), CopyError> {
        let metadata = source.symlink_metadata().map_err(io)?;
        if metadata.file_type().is_symlink() {
            return Err(CopyError::Unsupported);
        }
        if metadata.is_file() {
            return copy(source, destination, cancelled, on_bytes);
        }
        if metadata.is_dir() {
            return copy_directory(
                source,
                destination,
                token,
                item,
                cancelled,
                on_bytes,
                on_file_completed,
                on_file_discovered,
                on_directory_undo,
            );
        }
        Err(CopyError::Unsupported)
    }

    pub(super) fn copy_entry_with_retry_provenance(
        source: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
        on_directory_undo: &mut dyn FnMut(super::super::model::DirectoryCopyUndoReceipt),
        on_retry_provenance: &mut dyn FnMut(super::super::model::CopyDirectoryRetryProvenance),
    ) -> Result<(), CopyError> {
        let metadata = source.symlink_metadata().map_err(io)?;
        if metadata.file_type().is_symlink() { return Err(CopyError::Unsupported); }
        if metadata.is_file() { return copy(source, destination, cancelled, on_bytes); }
        if metadata.is_dir() {
            return copy_directory_with_collision_cleanup(
                source, destination, token, item, cancelled, on_bytes, on_file_completed,
                on_file_discovered, on_directory_undo, false, None, on_retry_provenance,
            );
        }
        Err(CopyError::Unsupported)
    }

    pub(super) fn copy_directory_with_retry_guard(
        source: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
        on_directory_undo: &mut dyn FnMut(super::super::model::DirectoryCopyUndoReceipt),
        retry_guard: Option<&super::super::model::CopyDirectoryRetryGuard>,
        on_retry_provenance: &mut dyn FnMut(super::super::model::CopyDirectoryRetryProvenance),
    ) -> Result<(), CopyError> {
        let metadata = source.symlink_metadata().map_err(io)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() { return Err(CopyError::Unsupported); }
        copy_directory_with_collision_cleanup(
            source, destination, token, item, cancelled, on_bytes, on_file_completed,
            on_file_discovered, on_directory_undo, false, retry_guard, on_retry_provenance,
        )
    }

    pub(super) fn copy_entry_for_recovery_keep_both(
        source: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
        on_directory_undo: &mut dyn FnMut(super::super::model::DirectoryCopyUndoReceipt),
    ) -> Result<(), CopyError> {
        let metadata = source.symlink_metadata().map_err(io)?;
        if metadata.file_type().is_symlink() {
            return Err(CopyError::Unsupported);
        }
        if metadata.is_file() {
            return copy(source, destination, cancelled, on_bytes);
        }
        if metadata.is_dir() {
            return copy_directory_with_collision_cleanup(
                source,
                destination,
                token,
                item,
                cancelled,
                on_bytes,
                on_file_completed,
                on_file_discovered,
                on_directory_undo,
                true,
                None,
                &mut |_| {},
            );
        }
        Err(CopyError::Unsupported)
    }

    /// The staging directory is never the visible destination.  A failure or
    /// cancellation therefore cannot produce a partial folder at the user's
    /// requested name.  We deliberately retain an incomplete staging tree:
    /// deleting a recursively populated directory by a name after a failure
    /// could delete files another process has placed there.
    fn copy_directory(
        source: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
        on_directory_undo: &mut dyn FnMut(super::super::model::DirectoryCopyUndoReceipt),
    ) -> Result<(), CopyError> {
        copy_directory_with_collision_cleanup(
            source,
            destination,
            token,
            item,
            cancelled,
            on_bytes,
            on_file_completed,
            on_file_discovered,
            on_directory_undo,
            false,
            None,
            &mut |_| {},
        )
    }

    fn copy_directory_with_collision_cleanup(
        source: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
        on_directory_undo: &mut dyn FnMut(super::super::model::DirectoryCopyUndoReceipt),
        retry_collision_after_verified_cleanup: bool,
        retry_guard: Option<&super::super::model::CopyDirectoryRetryGuard>,
        on_retry_provenance: &mut dyn FnMut(super::super::model::CopyDirectoryRetryProvenance),
    ) -> Result<(), CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        crate::operations::rename::validate_directory_copy_target(source, destination)
            .map_err(|error| CopyError::Io(format!("{error:?}")))?;
        let parent = destination.parent().ok_or(CopyError::Unsupported)?;
        let guarded_destination_route = if retry_guard.is_some() {
            Some(lock_destination_route_root_first(parent, |_| {})?)
        } else {
            None
        };
        let destination_parent_owner = if guarded_destination_route.is_none() {
            Some(open_pinned_destination_parent(parent)?)
        } else {
            None
        };
        let destination_parent = guarded_destination_route.as_ref().and_then(|route| route.last())
            .or(destination_parent_owner.as_ref()).ok_or(CopyError::Unsupported)?;
        // For an admitted retry, both routes are held and compared before a
        // fresh stage exists. Initial Copy keeps its established stage-first
        // sequence and records the identities of the handles it then uses.
        let (staging, staging_root, stage_root_identity, source_ancestry) = if let Some(guard) = retry_guard {
            let source_ancestry = lock_copy_source_ancestry_root_first(source)?;
            let source_root = source_ancestry.last().ok_or(CopyError::Unsupported)?;
            let source_identity = directory_identity(source_root)?;
            let parent_identity = directory_identity(destination_parent)?;
            let source_root_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                source_identity.volume,
                (u64::from(source_identity.index_high) << 32) | u64::from(source_identity.index_low),
            );
            let destination_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                parent_identity.volume,
                (u64::from(parent_identity.index_high) << 32) | u64::from(parent_identity.index_low),
            );
            if guard.item != item || guard.source != source || guard.destination != destination
                || guard.source_root_identity != source_root_identity
                || guard.destination_parent_identity != destination_parent_identity {
                return Err(CopyError::Io("directory Copy retry source root or destination parent changed".into()));
            }
        on_retry_provenance(super::super::model::CopyDirectoryRetryProvenance {
                item, source: source.to_path_buf(), destination: destination.to_path_buf(),
                source_root_identity, destination_parent_identity,
            });
            use std::os::windows::ffi::OsStrExt;
            let leaf = destination.file_name().ok_or(CopyError::Unsupported)?
                .encode_wide().collect::<Vec<_>>();
            match nt_probe_child_no_follow(destination_parent, &leaf)? {
                NtChildProbe::Absent => {}
                NtChildProbe::Present(handle) => {
                    unsafe { let _ = CloseHandle(handle); }
                    return Err(CopyError::DestinationExists);
                }
            }
            let (staging, staging_root) = create_staging_directory(destination_parent, parent, destination, token, item)?;
            let stage_root_identity = directory_identity_text(&staging_root).unwrap_or_default();
            (staging, staging_root, stage_root_identity, source_ancestry)
        } else {
            let (staging, staging_root) = create_staging_directory(destination_parent, parent, destination, token, item)?;
            let stage_root_identity = directory_identity_text(&staging_root).unwrap_or_default();
            // Hold the source route from its volume root through publication.
            let source_ancestry = match lock_copy_source_ancestry_root_first(source) {
                Ok(ancestry) => ancestry,
                Err(error) => return Err(retained_directory_stage_error(&staging, destination, &stage_root_identity, error)),
            };
            let source_root = source_ancestry.last().ok_or(CopyError::Unsupported)?;
            let source_identity = directory_identity(source_root)?;
            let parent_identity = directory_identity(destination_parent)?;
            let source_root_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                source_identity.volume,
                (u64::from(source_identity.index_high) << 32) | u64::from(source_identity.index_low),
            );
            let destination_parent_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                parent_identity.volume,
                (u64::from(parent_identity.index_high) << 32) | u64::from(parent_identity.index_low),
            );
            on_retry_provenance(super::super::model::CopyDirectoryRetryProvenance {
                item, source: source.to_path_buf(), destination: destination.to_path_buf(),
                source_root_identity, destination_parent_identity,
            });
            #[cfg(test)]
            if take_copy_directory_staging_failure_for_test(token, item) {
                return Err(retained_directory_stage_error(
                    &staging,
                    destination,
                    &stage_root_identity,
                    CopyError::Io("injected directory Copy staging failure".into()),
                ));
            }
            (staging, staging_root, stage_root_identity, source_ancestry)
        };
        let mut facts = Vec::new();
        let result = copy_directory_contents(
            token,
            item,
            source,
            &source_ancestry,
            &staging_root,
            &mut Vec::new(),
            &mut facts,
            cancelled,
            on_bytes,
            on_file_completed,
            on_file_discovered,
        );
        if let Err(error) = result {
            eprintln!(
                "[v6][operations] retaining incomplete folder-copy staging tree: {} ({error:?})",
                staging.display()
            );
            return Err(retained_directory_stage_error(&staging, destination, &stage_root_identity, error));
        }
        if cancelled.load(Ordering::Acquire) {
            eprintln!(
                "[v6][operations] retaining cancelled folder-copy staging tree: {}",
                staging.display()
            );
            return Err(retained_directory_stage_error(&staging, destination, &stage_root_identity, CopyError::Cancelled));
        }
        let root_identity = directory_identity_text(&staging_root)?;
        // The per-child creation handles have been released at this point. Keep
        // the deterministic test mutation inside that observable release window.
        #[cfg(test)]
        COPY_DIRECTORY_PREVERIFY_TEST_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook(&staging);
            }
        });
        // descendant handleをroot publishまで保持するとWindowsのrenameが失敗するため、
        // 解放後のtreeをcopy中に捕捉したidentity/content factsと照合してからpublishする。
        let before = match snapshot_copy_tree(&staging_root, &mut Vec::new(), cancelled) {
            Ok(snapshot) => snapshot,
            Err(error) => return Err(retained_directory_stage_error(&staging, destination, &stage_root_identity, error)),
        };
        if !copy_facts_match(&facts, &before.facts) {
            return Err(retained_directory_stage_error(&staging, destination, &stage_root_identity, CopyError::Unsupported));
        }
        let cleanup_facts = before
            .facts
            .iter()
            .map(|fact| StagedChildFact {
                relative_components: fact.relative.clone(),
                node_kind: if fact.directory {
                    crate::operations::journal::JournalDirectoryNodeKind::Directory
                } else {
                    crate::operations::journal::JournalDirectoryNodeKind::File
                },
                identity: fact.identity.clone(),
                content_fact: if fact.directory {
                    "directory".to_owned()
                } else {
                    format!("sha256:{}", fact.sha256)
                },
            })
            .collect::<Vec<_>>();
        drop(before);
        if cancelled.load(Ordering::Acquire) {
            return Err(retained_directory_stage_error(&staging, destination, &stage_root_identity, CopyError::Cancelled));
        }
        match publish_no_replace_handle(staging_root.0, destination) {
            Ok(()) => {}
            Err(CopyError::DestinationExists) => {
                if !retry_collision_after_verified_cleanup {
                    return Err(retained_directory_stage_error(
                        &staging,
                        destination,
                        &root_identity,
                        CopyError::DestinationExists,
                    ));
                }
                let cleanup = (|| -> Result<(), CopyError> {
                    let nodes = reopen_staged_children_by_facts(&staging_root, &cleanup_facts)?;
                    cleanup_staged_handles(&staging_root, &root_identity, nodes)?;
                    Ok(())
                })();
                drop(staging_root);
                if let Err(error) = cleanup {
                    return Err(staging_cleanup_partial(
                        &staging,
                        CopyError::DestinationExists,
                        error,
                    ));
                }
                if staging.exists() {
                    return Err(CopyError::PrePublishPartial(format!(
                        "verified collision-stage cleanup left {} present",
                        staging.display()
                    )));
                }
                return Err(CopyError::DestinationExists);
            }
            Err(error) => {
                return Err(directory_copy_unknown(
                    &staging, destination, &root_identity, &facts,
                    super::super::model::CopyDirectoryPublicationPhase::RenameOutcomeUncertain,
                    None, None, None, &format!("rename API result: {error:?}"),
                ));
            }
        }
        // The stage-root handle carries add-child access used during copying.
        // Release that access after publication so the final read-only pin can
        // make destination-name identity stable through verification.
        drop(staging_root);
        #[cfg(test)]
        COPY_DIRECTORY_POSTPUBLISH_TEST_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook(destination);
            }
        });
        #[cfg(test)]
        COPY_DIRECTORY_POSTPUBLISH_TEST_FAULT.with(|slot| match slot.replace(None) {
            Some(CopyDirectoryPostPublishTestFault::Cancel) => {
                cancelled.store(true, Ordering::Release);
            }
            Some(CopyDirectoryPostPublishTestFault::AddForeignChild) => {
                std::fs::write(destination.join("foreign-after-publish.bin"), b"foreign")
                    .expect("postpublish test mutation writes inside its fixture destination");
            }
            None => {}
        });
        if cancelled.load(Ordering::Acquire) {
            return Err(directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PostPublishCancelled,
                None, None, None, "cancellation observed after root publication",
            ));
        }
        let published_root = open_published_root(destination).map_err(|error| {
            directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PublishedRootOpenFailed,
                None, None, None, &format!("published root reopen: {error:?}"),
            )
        })?;
        let published_identity = directory_identity_text(&published_root).map_err(|error| {
            directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PublishedRootIdentityFailed,
                None, None, None, &format!("published root identity query: {error:?}"),
            )
        })?;
        let after = snapshot_copy_tree(&published_root, &mut Vec::new(), cancelled).map_err(|error| {
            let phase = if matches!(error, CopyError::Cancelled) {
                super::super::model::CopyDirectoryPublicationPhase::PostPublishCancelled
            } else {
                super::super::model::CopyDirectoryPublicationPhase::PublishedInventoryFailed
            };
            directory_copy_unknown(
                &staging, destination, &root_identity, &facts, phase,
                Some(&published_identity), None, None, &format!("published inventory: {error:?}"),
            )
        })?;
        let after_digest = copy_inventory_digest(&after.facts);
        if cancelled.load(Ordering::Acquire) {
            return Err(directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PostPublishCancelled,
                Some(&published_identity), Some(after.facts.len() as u64), Some(&after_digest),
                "cancellation observed during postpublication verification",
            ));
        }
        if published_identity != root_identity || !copy_facts_match(&facts, &after.facts) {
            return Err(directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PublishedInventoryMismatch,
                Some(&published_identity), Some(after.facts.len() as u64), Some(&after_digest),
                "published namespace, identity, or content commitment differs",
            ));
        }
        // Pin the destination name itself without delete/write sharing before
        // the final namespace pass. The identity check and namespace walk then
        // hold the path and each exact directory object through the success
        // decision.
        let destination_guard = open_pinned_directory_with_share(
            destination,
            false,
            FILE_SHARE_READ,
        ).map_err(|error| directory_copy_unknown(
            &staging, destination, &root_identity, &facts,
            super::super::model::CopyDirectoryPublicationPhase::PublishedRootOpenFailed,
            Some(&published_identity), Some(after.facts.len() as u64), Some(&after_digest),
            &format!("final destination root pin: {error:?}"),
        ))?;
        let guarded_identity = directory_identity_text(&destination_guard).map_err(|error| {
            directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PublishedRootIdentityFailed,
                None, Some(after.facts.len() as u64), Some(&after_digest),
                &format!("final destination root identity: {error:?}"),
            )
        })?;
        if guarded_identity != published_identity {
            return Err(directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PublishedRootIdentityFailed,
                Some(&guarded_identity), Some(after.facts.len() as u64), Some(&after_digest),
                "destination name no longer identifies the published root",
            ));
        }
        if let Err(error) = verify_copy_directory_namespaces(&destination_guard, &after, cancelled) {
            let phase = if matches!(error, CopyError::Cancelled) {
                super::super::model::CopyDirectoryPublicationPhase::PostPublishCancelled
            } else {
                super::super::model::CopyDirectoryPublicationPhase::PublishedInventoryMismatch
            };
            return Err(directory_copy_unknown(
                &staging, destination, &root_identity, &facts, phase,
                Some(&guarded_identity), Some(after.facts.len() as u64), Some(&after_digest),
                &format!("final exact namespace verification: {error:?}"),
            ));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(directory_copy_unknown(
                &staging, destination, &root_identity, &facts,
                super::super::model::CopyDirectoryPublicationPhase::PostPublishCancelled,
                Some(&published_identity), Some(after.facts.len() as u64), Some(&after_digest),
                "cancellation observed before directory Copy success",
            ));
        }
        if let Some(receipt) = directory_copy_undo_receipt(destination, &published_identity, &after) {
            on_directory_undo(receipt);
        }
        drop(after);
        Ok(())
    }

    fn copy_directory_contents(
        token: AttemptToken,
        item: ItemId,
        source: &Path,
        _source_ancestry: &[DirectoryLock],
        destination_parent: &DirectoryLock,
        relative: &mut Vec<Vec<u16>>,
        facts: &mut Vec<OrdinaryCopyFact>,
        cancelled: &AtomicBool,
        on_bytes: &mut dyn FnMut(u64),
        on_file_completed: &mut dyn FnMut(),
        on_file_discovered: &mut dyn FnMut(u64, u64),
    ) -> Result<(), CopyError> {
        // Holding the exact source directory open without FILE_SHARE_DELETE
        // prevents it being replaced by a junction after the no-follow probe.
        let _source_lock = lock_copy_source_directory(source)?;
        let mut children = Vec::new();
        // 各階層を列挙してから個別に admit するため、tree 全体の snapshot は保証しない。
        // Enumeration と open の間に変化した leaf は no-follow handle で再判定する。
        for entry in std::fs::read_dir(source).map_err(io)? {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            let entry = entry.map_err(io)?;
            let source_child = entry.path();
            let metadata = source_child.symlink_metadata().map_err(io)?;
            if metadata.file_type().is_symlink() {
                return Err(CopyError::Unsupported);
            }
            children.push((source_child, entry.file_name(), metadata));
        }
        // One directory level is enumerated before its first copy starts. This
        // gives folder progress a useful, explicitly lower-bound denominator
        // without blocking on a whole-tree pre-scan or flooding the UI for a
        // single large directory.
        let mut discovered_items = 0_u64;
        let mut discovered_bytes = 0_u64;
        for (_, _, metadata) in &children {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            if metadata.is_file() {
                discovered_items = discovered_items.saturating_add(1);
                discovered_bytes = discovered_bytes.saturating_add(metadata.len());
            }
        }
        if discovered_items > 0 {
            on_file_discovered(discovered_items, discovered_bytes);
        }
        for (source_child, child_name, metadata) in children {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            use std::os::windows::ffi::OsStrExt;
            let component = child_name.encode_wide().collect::<Vec<_>>();
            relative.push(component.clone());
            if metadata.is_file() {
                #[cfg(test)]
                COPY_DIRECTORY_SOURCE_OPEN_TEST_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().as_mut() {
                        hook(&source_child);
                    }
                });
                let source_file = open_copy_regular_file_nofollow(&source_child)?;
                let mut target = create_staged_file(destination_parent, &component)?;
                let identity = file_identity_text(&target)?;
                #[cfg(test)]
                run_copy_directory_stage_write_lock_hook_for_test(token, item, &target);
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                let mut length = 0_u64;
                let mut buffer = [0_u8; 64 * 1024];
                let mut source_file = source_file;
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(CopyError::Cancelled);
                    }
                    let read = std::io::Read::read(&mut source_file, &mut buffer).map_err(io)?;
                    if read == 0 { break; }
                    let write_result = std::io::Write::write_all(&mut target, &buffer[..read]);
                    #[cfg(test)]
                    if let Err(error) = &write_result {
                        COPY_DIRECTORY_STAGE_WRITE_ERROR_TEST.with(|slot| slot.set(error.raw_os_error()));
                    }
                    write_result.map_err(io)?;
                    hasher.update(&buffer[..read]);
                    length = length.saturating_add(read as u64);
                    on_bytes(read as u64);
                }
                target.sync_all().map_err(io)?;
                facts.push(OrdinaryCopyFact {
                    relative: relative.clone(),
                    directory: false,
                    identity,
                    length,
                    sha256: format!("{:x}", hasher.finalize()),
                });
                drop(target);
                drop(source_file);
                on_file_completed();
            } else if metadata.is_dir() {
                let target = create_staged_directory(
                    destination_parent,
                    &component,
                    true,
                    FILE_SHARE_READ.0,
                )?;
                facts.push(OrdinaryCopyFact {
                    relative: relative.clone(),
                    directory: true,
                    identity: directory_identity_text(&target)?,
                    length: 0,
                    sha256: String::new(),
                });
                copy_directory_contents(
                    token,
                    item,
                    &source_child,
                    _source_ancestry,
                    &target,
                    relative,
                    facts,
                    cancelled,
                    on_bytes,
                    on_file_completed,
                    on_file_discovered,
                )?;
            } else {
                return Err(CopyError::Unsupported);
            }
            relative.pop();
        }
        Ok(())
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct OrdinaryCopyFact {
        relative: Vec<Vec<u16>>,
        directory: bool,
        identity: String,
        length: u64,
        sha256: String,
    }

    enum RetainedCopyChild {
        Directory { relative: Vec<Vec<u16>>, handle: DirectoryLock },
        File(std::fs::File),
    }

    struct CopyDirectoryInventory {
        relative: Vec<Vec<u16>>,
        children: Vec<(Vec<u16>, bool, String)>,
    }

    struct CopyTreeSnapshot {
        facts: Vec<OrdinaryCopyFact>,
        directories: Vec<CopyDirectoryInventory>,
        // Retaining every opened object prevents a later sibling hash from
        // silently outliving the identity that was actually verified.
        _retained: Vec<RetainedCopyChild>,
    }

    fn file_identity_text(file: &std::fs::File) -> Result<String, CopyError> {
        let identity = regular_file_identity(file)?;
        Ok(format!("{}:{:08x}{:08x}", identity.volume, identity.index_high, identity.index_low))
    }

    fn copy_facts_match(expected: &[OrdinaryCopyFact], actual: &[OrdinaryCopyFact]) -> bool {
        let mut expected = expected.to_vec();
        let mut actual = actual.to_vec();
        expected.sort_by(|a, b| a.relative.cmp(&b.relative));
        actual.sort_by(|a, b| a.relative.cmp(&b.relative));
        expected == actual
    }

    fn copy_inventory_digest(facts: &[OrdinaryCopyFact]) -> String {
        use sha2::Digest;
        let mut canonical = facts.to_vec();
        canonical.sort_by(|left, right| left.relative.cmp(&right.relative));
        let mut digest = sha2::Sha256::new();
        digest.update(b"gpui-filer-ordinary-directory-copy-inventory-v1\0");
        digest.update((canonical.len() as u64).to_le_bytes());
        for fact in canonical {
            digest.update([u8::from(fact.directory)]);
            digest.update((fact.relative.len() as u64).to_le_bytes());
            for component in &fact.relative {
                digest.update((component.len() as u64).to_le_bytes());
                for unit in component {
                    digest.update(unit.to_le_bytes());
                }
            }
            digest.update((fact.identity.len() as u64).to_le_bytes());
            digest.update(fact.identity.as_bytes());
            digest.update(fact.length.to_le_bytes());
            digest.update((fact.sha256.len() as u64).to_le_bytes());
            digest.update(fact.sha256.as_bytes());
        }
        format!("{:x}", digest.finalize())
    }

    fn directory_copy_unknown(
        staging: &Path,
        destination: &Path,
        root_identity: &str,
        facts: &[OrdinaryCopyFact],
        phase: super::super::model::CopyDirectoryPublicationPhase,
        observed_root_identity: Option<&str>,
        observed_node_count: Option<u64>,
        observed_inventory_sha256: Option<&str>,
        detail: &str,
    ) -> CopyError {
        use super::super::model::CopyDirectoryPublicationEvidence as Evidence;
        let detail = detail.chars().take(384).collect();
        CopyError::UnknownDirectoryPublication(Box::new(Evidence {
            stage_path_units: staging.as_os_str().encode_wide().collect(),
            destination_path_units: destination.as_os_str().encode_wide().collect(),
            expected_root_identity: root_identity.to_string(),
            expected_node_count: facts.len() as u64,
            expected_inventory_sha256: copy_inventory_digest(facts),
            observed_root_identity: observed_root_identity.map(str::to_owned),
            observed_node_count,
            observed_inventory_sha256: observed_inventory_sha256.map(str::to_owned),
            phase,
            detail,
        }))
    }

    fn retained_directory_stage_error(staging: &Path, destination: &Path, identity: &str, cause: CopyError) -> CopyError {
        use super::super::model::{CopyDirectoryStageEvidence, CopyDirectoryStagePhase};
        use std::os::windows::ffi::OsStrExt;
        let stage_path_units = staging.as_os_str().encode_wide().collect::<Vec<_>>();
        let destination_path_units = destination.as_os_str().encode_wide().collect::<Vec<_>>();
        if stage_path_units.len() > 8192 || destination_path_units.len() > 8192 { return cause; }
        let phase = if matches!(cause, CopyError::Cancelled) { CopyDirectoryStagePhase::StagingCancelled }
            else { CopyDirectoryStagePhase::StagingFailed };
        let detail = format!("{cause:?}").chars().take(384).collect();
        CopyError::DirectoryCopyStageRetained {
            evidence: CopyDirectoryStageEvidence { stage_path_units, destination_path_units,
                expected_root_identity: identity.to_owned(), phase, detail },
            cause: Box::new(cause),
        }
    }

    fn snapshot_copy_tree(
        root: &DirectoryLock,
        relative: &mut Vec<Vec<u16>>,
        cancelled: &AtomicBool,
    ) -> Result<CopyTreeSnapshot, CopyError> {
        use sha2::Digest;
        use std::os::windows::io::FromRawHandle;
        let mut snapshot = CopyTreeSnapshot { facts: Vec::new(), directories: Vec::new(), _retained: Vec::new() };
        let mut entries = enumerate_directory_handle_with_cancel(
            root,
            1_000_000,
            64 * 1024 * 1024,
            Some(cancelled),
        )?;
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut immediate_children = Vec::with_capacity(entries.len());
        for (name, directory) in entries {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            relative.push(name.clone());
            let raw = nt_open_child(root, &name, directory)?;
            if directory {
                let child = DirectoryLock(raw);
                let identity = directory_identity_text(&child)?;
                immediate_children.push((name.clone(), true, identity.clone()));
                snapshot.facts.push(OrdinaryCopyFact {
                    relative: relative.clone(),
                    directory: true,
                    identity,
                    length: 0,
                    sha256: String::new(),
                });
                let nested = snapshot_copy_tree(&child, relative, cancelled)?;
                snapshot.facts.extend(nested.facts);
                snapshot.directories.extend(nested.directories);
                snapshot._retained.extend(nested._retained);
                snapshot._retained.push(RetainedCopyChild::Directory {
                    relative: relative.clone(),
                    handle: child,
                });
            } else {
                let file = unsafe { std::fs::File::from_raw_handle(raw.0) };
                let identity = file_identity_text(&file)?;
                immediate_children.push((name.clone(), false, identity.clone()));
                let mut reader = file;
                let mut hasher = sha2::Sha256::new();
                let mut length = 0_u64;
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(CopyError::Cancelled);
                    }
                    let read = std::io::Read::read(&mut reader, &mut buffer).map_err(io)?;
                    if read == 0 { break; }
                    hasher.update(&buffer[..read]);
                    length = length.saturating_add(read as u64);
                    #[cfg(test)]
                    COPY_DIRECTORY_SCAN_TEST_HOOK.with(|slot| {
                        let mut slot = slot.borrow_mut();
                        if slot.as_mut().is_some_and(|hook| hook(relative)) {
                            slot.take();
                        }
                    });
                }
                if cancelled.load(Ordering::Acquire) {
                    return Err(CopyError::Cancelled);
                }
                snapshot.facts.push(OrdinaryCopyFact {
                    relative: relative.clone(),
                    directory: false,
                    identity,
                    length,
                    sha256: format!("{:x}", hasher.finalize()),
                });
                snapshot._retained.push(RetainedCopyChild::File(reader));
            }
            relative.pop();
        }
        snapshot.directories.push(CopyDirectoryInventory {
            relative: relative.clone(),
            children: immediate_children,
        });
        Ok(snapshot)
    }

    fn verify_copy_directory_namespaces(
        root: &DirectoryLock,
        snapshot: &CopyTreeSnapshot,
        cancelled: &AtomicBool,
    ) -> Result<(), CopyError> {
        use std::os::windows::io::FromRawHandle;
        #[cfg(test)]
        let mut previous: Option<(&[Vec<u16>], &DirectoryLock)> = None;
        for inventory in &snapshot.directories {
            if cancelled.load(Ordering::Acquire) {
                return Err(CopyError::Cancelled);
            }
            let directory = if inventory.relative.is_empty() {
                root
            } else {
                match snapshot._retained.iter().find_map(|retained| match retained {
                    RetainedCopyChild::Directory { relative, handle } if relative == &inventory.relative => Some(handle),
                    _ => None,
                }) {
                    Some(handle) => handle,
                    None => return Err(CopyError::Unsupported),
                }
            };
            #[cfg(test)]
            if let Some((previous_relative, previous_directory)) = previous {
                COPY_DIRECTORY_NAMESPACE_TEST_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().take() {
                        hook(previous_relative, &inventory.relative, previous_directory);
                    }
                });
            }
            let mut observed = enumerate_directory_handle_with_cancel(
                directory,
                1_000_000,
                64 * 1024 * 1024,
                Some(cancelled),
            )?;
            observed.sort_by(|left, right| left.0.cmp(&right.0));
            if observed.len() != inventory.children.len() {
                return Err(CopyError::Unsupported);
            }
            for ((name, is_directory), (expected_name, expected_directory, expected_identity))
                in observed.iter().zip(&inventory.children)
            {
                if name != expected_name || is_directory != expected_directory {
                    return Err(CopyError::Unsupported);
                }
                let raw = nt_open_child(directory, name, *is_directory)?;
                let actual_identity = if *is_directory {
                    let child = DirectoryLock(raw);
                    directory_identity_text(&child)?
                } else {
                    let file = unsafe { std::fs::File::from_raw_handle(raw.0) };
                    file_identity_text(&file)?
                };
                if actual_identity != *expected_identity {
                    return Err(CopyError::Unsupported);
                }
            }
            #[cfg(test)]
            {
                previous = Some((&inventory.relative, directory));
            }
        }
        Ok(())
    }

    fn create_staging_directory(
        parent_handle: &DirectoryLock,
        parent_path: &Path,
        destination: &Path,
        token: AttemptToken,
        item: ItemId,
    ) -> Result<(PathBuf, DirectoryLock), CopyError> {
        use std::os::windows::ffi::OsStrExt;

        for nonce in 0..10_000_u32 {
            let name = format!(
                ".gpui-filer-copy-{}-{}-{}-{}-{nonce}",
                std::process::id(),
                token.job_id().value(),
                token.attempt_id().value(),
                item.0,
            );
            let component = std::ffi::OsStr::new(&name)
                .encode_wide()
                .collect::<Vec<_>>();
            match nt_open_or_create_child(
                parent_handle,
                &component,
                true,
                true,
                NT_FILE_CREATE,
                FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
                true,
            ) {
                Ok(handle) => {
                    // Wrap the successful native handle before validation so every error path
                    // releases the stage lease without reopening the path.
                    let root = DirectoryLock(handle);
                    let staging = parent_path.join(name);
                    #[cfg(test)]
                    if COPY_DIRECTORY_FAIL_STAGE_ROOT_IDENTITY.with(|slot| slot.replace(false)) {
                        return Err(unverified_created_stage_error(
                            &staging, destination, "injected stage-root identity failure"));
                    }
                    if let Err(error) = directory_identity(&root) {
                        let cleanup = set_raw_handle_delete_disposition(root.0);
                        return Err(unverified_created_stage_error(
                            &staging,
                            destination,
                            &format!(
                                "stage-root identity query failed: {error:?}; nonrecursive handle delete-disposition request: {cleanup:?}"
                            ),
                        ));
                    }
                    return Ok((staging, root));
                }
                Err(CopyError::DestinationExists) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(CopyError::Unsupported)
    }

    fn directory_copy_undo_receipt(
        destination: &Path,
        root_identity: &str,
        snapshot: &CopyTreeSnapshot,
    ) -> Option<super::super::model::DirectoryCopyUndoReceipt> {
        use super::super::model::{DirectoryCopyUndoEntry, DirectoryCopyUndoReceipt};
        if snapshot.facts.len() > SOURCE_TREE_MAX_ENTRIES {
            return None;
        }
        let mut name_bytes = 0usize;
        let mut evidence_bytes = root_identity.len();
        let mut entries = Vec::with_capacity(snapshot.facts.len());
        for fact in &snapshot.facts {
            if fact.relative.is_empty() || fact.relative.len() > SOURCE_TREE_MAX_DEPTH {
                return None;
            }
            for component in &fact.relative {
                if component.is_empty() || component.iter().any(|unit| *unit == 0 || *unit == b'/' as u16 || *unit == b'\\' as u16 || *unit == b':' as u16) {
                    return None;
                }
                name_bytes = name_bytes.checked_add(component.len().checked_mul(2)?)?;
                if name_bytes > SOURCE_TREE_MAX_NAME_BYTES { return None; }
            }
            let identity = fact.identity.clone();
            evidence_bytes = evidence_bytes.checked_add(identity.len())?;
            let (signature, content_sha256) = if fact.directory {
                (None, None)
            } else {
                let file = snapshot._retained.iter().find_map(|retained| match retained {
                    RetainedCopyChild::File(file) if file_identity_text(file).ok().as_deref() == Some(fact.identity.as_str()) => Some(file),
                    _ => None,
                })?;
                let signature = leaf_signature(file).ok()?;
                if signature.number_of_links != 1 || signature.change_time == 0 { return None; }
                evidence_bytes = evidence_bytes.checked_add(fact.sha256.len())?;
                (Some(signature), Some(fact.sha256.clone()))
            };
            if evidence_bytes > SOURCE_TREE_MAX_RECORD_BYTES { return None; }
            entries.push(DirectoryCopyUndoEntry {
                relative_components: fact.relative.clone(),
                kind: if fact.directory { crate::operations::journal::JournalDirectoryNodeKind::Directory } else { crate::operations::journal::JournalDirectoryNodeKind::File },
                identity,
                signature,
                length: fact.length,
                content_sha256,
            });
        }
        let parent = destination.parent()?;
        let mut paths = Vec::new();
        let route = lock_destination_route_root_first(parent, |path| paths.push(path.to_path_buf())).ok()?;
        let destination_parent_route = parent_route_proof(&paths, &route).ok()?;
        Some(DirectoryCopyUndoReceipt { destination: destination.to_path_buf(), destination_parent_route, root_identity: root_identity.to_owned(), entries })
    }

    #[cfg(test)]
    pub(super) fn copy_directory_for_test(
        source: &Path,
        destination: &Path,
    ) -> Result<(), CopyError> {
        copy_directory(
            source,
            destination,
            AttemptToken::fixture(),
            ItemId(0),
            &AtomicBool::new(false),
            &mut |_| {},
            &mut || {},
            &mut |_, _| {},
            &mut |_| {},
        )
    }

    #[cfg(test)]
    pub(super) fn copy_directory_for_test_with_undo(
        source: &Path,
        destination: &Path,
    ) -> Result<Option<super::super::model::DirectoryCopyUndoReceipt>, CopyError> {
        let mut candidate = None;
        copy_directory(
            source, destination, AttemptToken::fixture(), ItemId(0),
            &AtomicBool::new(false), &mut |_| {}, &mut || {}, &mut |_, _| {},
            &mut |receipt| candidate = Some(receipt),
        )?;
        Ok(candidate)
    }

    pub(super) fn undo_directory_copy(
        receipt: &crate::operations::model::DirectoryCopyUndoReceipt,
        custody: super::DirectoryCopyUndoCustodySlot,
    ) -> super::DirectoryCopyUndoResult {
        use crate::operations::journal::JournalDirectoryNodeKind;
        use std::time::{Duration, Instant};
        fn refuse(error: impl std::fmt::Display) -> super::DirectoryCopyUndoResult {
            super::DirectoryCopyUndoResult::Refused(error.to_string())
        }
        let parent_path = match receipt.destination.parent() { Some(path) => path, None => return refuse("destination parent is missing") };
        if receipt.destination_parent_route.last().is_none_or(|parent| parent.path != parent_path)
            || receipt.entries.len() > SOURCE_TREE_MAX_ENTRIES
            || receipt.root_identity.is_empty()
        { return refuse("directory Copy receipt is malformed"); }
        let mut paths = Vec::new();
        let route = match lock_destination_route_root_first(parent_path, |path| paths.push(path.to_path_buf())) {
            Ok(route) => route,
            Err(error) => return refuse(format!("destination parent route: {error:?}")),
        };
        let actual_route = match parent_route_proof(&paths, &route) {
            Ok(route) => route,
            Err(error) => return refuse(format!("destination parent route identity: {error:?}")),
        };
        if actual_route != receipt.destination_parent_route { return refuse("destination parent route changed"); }
        let parent = match route.last() { Some(parent) => parent, None => return refuse("destination parent route is empty") };
        let Some(leaf) = receipt.destination.file_name().map(|name| name.encode_wide().collect::<Vec<_>>()) else { return refuse("destination root name is missing"); };
        let root_handle = match nt_open_or_create_child(parent, &leaf, true, true, NT_FILE_OPEN, FILE_SHARE_READ.0, false) {
            Ok(handle) => handle,
            Err(error) => return refuse(format!("destination root open: {error:?}")),
        };
        let root = DirectoryLock(root_handle);
        let root_identity = match directory_identity_text(&root) { Ok(identity) => identity, Err(error) => return refuse(format!("destination root identity: {error:?}")) };
        if root_identity != receipt.root_identity { return refuse("destination root identity changed"); }
        let mut facts = Vec::with_capacity(receipt.entries.len());
        for entry in &receipt.entries {
            let content_fact = match &entry.kind {
                JournalDirectoryNodeKind::Directory => "directory".to_owned(),
                JournalDirectoryNodeKind::File => format!("sha256:{}", entry.content_sha256.as_deref().unwrap_or_default()),
                JournalDirectoryNodeKind::Unknown => return refuse("unknown directory entry kind"),
            };
            facts.push(StagedChildFact {
                relative_components: entry.relative_components.clone(),
                node_kind: entry.kind.clone(),
                identity: entry.identity.clone(),
                content_fact,
            });
        }
        let handles = match reopen_staged_children_by_facts(&root, &facts) {
            Ok(handles) => handles,
            Err(error) => return refuse(format!("directory Copy preflight: {error:?}")),
        };
        if handles.len() != receipt.entries.len() {
            return refuse("directory Copy entry inventory changed");
        }
        // The snapshot's traversal order is not the custody helper's delete order.
        // Match by the captured relative name so each retained handle is checked
        // against its own exact fact before any disposition is attempted.
        for entry in &receipt.entries {
            let Some(node) = handles.iter().find(|node| node.relative_components == entry.relative_components) else {
                return refuse("directory Copy entry is missing");
            };
            if entry.kind != node.node_kind || entry.identity != node.identity {
                return refuse("directory Copy entry identity changed");
            }
            match (&entry.kind, &node.handle) {
                (JournalDirectoryNodeKind::Directory, PinnedSourceHandle::Directory(_)) if entry.signature.is_none() && entry.content_sha256.is_none() && entry.length == 0 => {}
                (JournalDirectoryNodeKind::File, PinnedSourceHandle::File(file)) => {
                    if entry.signature.is_none_or(|expected| leaf_signature(file).ok() != Some(expected))
                        || file.metadata().ok().is_none_or(|metadata| metadata.len() != entry.length)
                    { return refuse("directory Copy file signature changed"); }
                }
                (JournalDirectoryNodeKind::Unknown, _) => return refuse("unknown directory entry kind"),
                _ => return refuse("directory Copy entry kind changed"),
            }
        }
        let (volume, file_index) = match receipt.destination_parent_route.last().and_then(|parent| parent.identity.windows_parts()) {
            Some(parts) => parts,
            None => return refuse("destination parent identity is unsupported"),
        };
        let staged = VerifiedStagedTree {
            root_path: receipt.destination.clone(),
            root_identity: receipt.root_identity.clone(),
            root_chain: route,
            root,
            nodes: facts,
            handles,
        };
        let expected_parent = crate::operations::journal::JournalSevenZipExtractIdentity { volume, file_index };
        let deadline = Instant::now() + Duration::from_secs(30);
        #[cfg(test)]
        let cleanup_result = match custody.take_cleanup_close_fault_for_test() {
            Some((root, injected_close_handle)) => super::windows::extract_cleanup::delete_staged_tree_close_fault_for_test(
                staged, &expected_parent, parent_path, deadline, root, injected_close_handle.as_ref(),
            ),
            None => super::windows::extract_cleanup::delete_staged_tree(
                staged, &expected_parent, parent_path, deadline,
            ),
        };
        #[cfg(not(test))]
        let cleanup_result = super::windows::extract_cleanup::delete_staged_tree(
            staged, &expected_parent, parent_path, deadline,
        );
        match cleanup_result {
            Ok(()) => super::DirectoryCopyUndoResult::Removed,
            Err(failure) => {
                let reason = failure.reason;
                let (passive_custody, mutated, uncertain) = failure.into_passive_undo_custody();
                if mutated || uncertain {
                    custody.install(passive_custody);
                    if mutated {
                        super::DirectoryCopyUndoResult::Partial(format!("directory Copy cleanup stopped after a disposition: {reason}"))
                    } else {
                        super::DirectoryCopyUndoResult::Uncertain(format!("directory Copy cleanup effect is uncertain: {reason}"))
                    }
                } else {
                    // No disposition was accepted and every native handle was
                    // converted to a known owner; dropping closes only those
                    // known handles before the worker returns.
                    drop(passive_custody);
                    super::DirectoryCopyUndoResult::Refused(format!("directory Copy cleanup refused before mutation: {reason}"))
                }
            }
        }
    }

    fn preflight_pinned_source_tree_for_cross_volume_undo(
        root: &Path,
        cancelled: &AtomicBool,
        expected_parent_identity: &str,
        expected_root_identity: &str,
    ) -> Result<PinnedSourceTree, CopyError> {
        // Undo must retain DELETE custody for each exact manifest object and
        // hash files while pinning so staging cannot publish unchecked bytes.
        preflight_pinned_source_tree_with_limits_and_shared_parent(
            root,
            cancelled,
            SourceTreeLimits::default(),
            Some(expected_parent_identity),
            Some(expected_root_identity),
            true,
            true,
            false,
            None,
        )
    }

    #[cfg(test)]
    pub(super) fn copy_directory_for_test_with_held_file(
        source: &Path,
        destination: &Path,
        held_path: &Path,
        held_file: &std::fs::File,
    ) -> Result<(), CopyError> {
        let retained_duplicate = held_file.try_clone().map_err(io)?;
        COPY_DIRECTORY_HELD_SOURCE_FILE_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace((held_path.to_path_buf(), retained_duplicate)).is_none());
        });
        let result = copy_directory(
            source,
            destination,
            AttemptToken::fixture(),
            ItemId(0),
            &AtomicBool::new(false),
            &mut |_| {},
            &mut || {},
            &mut |_, _| {},
            &mut |_| {},
        );
        COPY_DIRECTORY_HELD_SOURCE_FILE_TEST_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
        result
    }

    #[cfg(test)]
    pub(super) fn open_directory_for_child_and_disposition_test(
        path: &Path,
    ) -> Result<DirectoryLock, CopyError> {
        let path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_LIST_DIRECTORY.0
                    | FILE_READ_ATTRIBUTES.0
                    | ::windows::Win32::Storage::FileSystem::FILE_ADD_FILE.0
                    | DELETE.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let directory = DirectoryLock(handle);
        directory_identity(&directory)?;
        Ok(directory)
    }

    fn open_staged_child_directory_identity(
        staged: &VerifiedStagedTree,
        destination: &Path,
    ) -> Result<String, CopyError> {
        use std::os::windows::ffi::OsStrExt;
        if destination.parent() != staged.root_path.parent() {
            return Err(CopyError::Unsupported);
        }
        let parent = staged.root_chain.last().ok_or(CopyError::Unsupported)?;
        let component = destination.file_name().ok_or(CopyError::Unsupported)?
            .encode_wide().collect::<Vec<_>>();
        let child = DirectoryLock(nt_open_child(parent, &component, true)?);
        directory_identity_text(&child)
    }

    #[cfg(test)]
    pub(super) fn create_child_in_directory_for_test(
        directory: &DirectoryLock,
        name: &std::ffi::OsStr,
    ) -> Result<std::fs::File, CopyError> {
        use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
        let component = name.encode_wide().collect::<Vec<_>>();
        let handle = nt_create_child(directory, &component, false, true, FILE_SHARE_READ.0)?;
        Ok(unsafe { std::fs::File::from_raw_handle(handle.0) })
    }

    fn unverified_created_stage_error(stage: &Path, destination: &Path, detail: &str) -> CopyError {
        use super::super::model::{CopyDirectoryStageEvidence, CopyDirectoryStagePhase};
        use std::os::windows::ffi::OsStrExt;
        let stage_path_units = stage.as_os_str().encode_wide().collect::<Vec<_>>();
        let destination_path_units = destination.as_os_str().encode_wide().collect::<Vec<_>>();
        let cause = CopyError::Unknown(detail.chars().take(384).collect());
        if stage_path_units.is_empty() || destination_path_units.is_empty()
            || stage_path_units.len() > 8192 || destination_path_units.len() > 8192
        {
            return cause;
        }
        CopyError::DirectoryCopyStageRetained {
            evidence: CopyDirectoryStageEvidence {
                stage_path_units,
                destination_path_units,
                expected_root_identity: String::new(),
                phase: CopyDirectoryStagePhase::CreatedRootIdentityUnverified,
                detail: detail.chars().take(384).collect(),
            },
            cause: Box::new(cause),
        }
    }
    fn lock_directory(path: &Path) -> Result<DirectoryLock, CopyError> {
        let path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(handle, &mut info).map_err(io)?;
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(CopyError::Unsupported);
        }
        Ok(DirectoryLock(handle))
    }
    fn lock_copy_source_ancestry_root_first(
        source: &Path,
    ) -> Result<Vec<DirectoryLock>, CopyError> {
        let mut paths: Vec<_> = source.ancestors().collect();
        paths.reverse();
        let mut locks = Vec::with_capacity(paths.len());
        for path in paths {
            locks.push(lock_copy_source_directory(path)?);
        }
        Ok(locks)
    }
    fn lock_copy_source_directory(path: &Path) -> Result<DirectoryLock, CopyError> {
        let path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(io)?
        };
        let directory = DirectoryLock(handle);
        // FILE_SHARE_READ excludes data-write/delete opens while this exact
        // ordinary directory identity is being enumerated and traversed.
        directory_identity(&directory)?;
        Ok(directory)
    }

    #[cfg(test)]
    pub(super) fn probe_copy_source_directory_lock_for_test(path: &Path) -> Result<(), String> {
        lock_copy_source_directory(path)
            .map(drop)
            .map_err(|error| format!("{error:?}"))
    }
    fn open_copy_regular_file_nofollow(source: &Path) -> Result<std::fs::File, CopyError> {
        #[cfg(test)]
        let held = COPY_DIRECTORY_HELD_SOURCE_FILE_TEST_HOOK.with(|slot| {
            let slot = slot.borrow();
            slot.as_ref()
                .filter(|(held_path, _)| held_path == source)
                .map(|(_, file)| file.try_clone())
        });
        #[cfg(test)]
        if let Some(held) = held {
            let file = held.map_err(io)?;
            // DuplicateHandle retains the same Windows file object/access while
            // the caller keeps its original DELETE-capable handle through cleanup.
            regular_file_identity(&file)?;
            return Ok(file);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(source)
            .map_err(io)?;
        // Validate the retained handle used to supply all copied bytes.
        regular_file_identity(&file)?;
        Ok(file)
    }
    fn lock_non_reparse_ancestors(path: &Path) -> Result<Vec<DirectoryLock>, CopyError> {
        let mut locks = Vec::new();
        let mut cursor = Some(path);
        while let Some(directory) = cursor {
            locks.push(lock_directory(directory)?);
            cursor = directory.parent();
        }
        Ok(locks)
    }
    fn parent_route_proof(
        paths: &[PathBuf],
        handles: &[DirectoryLock],
    ) -> Result<Vec<crate::operations::model::SourceCleanupParentIdentity>, CopyError> {
        if paths.len() != handles.len() || paths.is_empty() {
            return Err(CopyError::Unsupported);
        }
        paths.iter().zip(handles).map(|(path, handle)| {
            let identity = directory_identity(handle)?;
            Ok(crate::operations::model::SourceCleanupParentIdentity {
                path: path.clone(),
                identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                    identity.volume,
                    (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
                ),
            })
        }).collect()
    }

    fn lock_destination_route_root_first(
        path: &Path,
        mut after_lock: impl FnMut(&Path),
    ) -> Result<Vec<DirectoryLock>, CopyError> {
        let mut paths: Vec<_> = path.ancestors().collect();
        paths.reverse();
        let mut locks = Vec::with_capacity(paths.len());
        for directory in paths {
            // Access attributes explicitly: a zero-access handle is not a
            // reliable Windows rename lease on all filesystems.
            locks.push(open_pinned_directory_with_share(
                directory,
                false,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            )?);
            after_lock(directory);
        }
        Ok(locks)
    }
    pub(super) fn lock_virtual_destination(
        path: &Path,
        expected: Option<&crate::filesystem_identity::FilesystemIdentity>,
    ) -> Result<Vec<DirectoryLock>, CopyError> {
        lock_virtual_destination_with_hook(path, expected, |_| {})
    }
    fn lock_virtual_destination_with_hook(
        path: &Path,
        expected: Option<&crate::filesystem_identity::FilesystemIdentity>,
        after_lock: impl FnMut(&Path),
    ) -> Result<Vec<DirectoryLock>, CopyError> {
        let route = lock_destination_route_root_first(path, after_lock)?;
        let destination = route.last().ok_or(CopyError::Unsupported)?;
        let identity = directory_identity(destination)?;
        let actual = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            identity.volume,
            (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
        );
        if expected.is_some_and(|expected| expected != &actual) {
            return Err(CopyError::GuardRejected(
                "virtual clipboard destination identity changed".into(),
            ));
        }
        Ok(route)
    }
    #[cfg(test)]
    pub(super) fn lock_virtual_destination_with_hook_for_test(
        path: &Path,
        expected: &crate::filesystem_identity::FilesystemIdentity,
        after_lock: impl FnMut(&Path),
    ) -> Result<Vec<DirectoryLock>, CopyError> {
        lock_virtual_destination_with_hook(path, Some(expected), after_lock)
    }
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    fn io(error: impl std::fmt::Display) -> CopyError {
        CopyError::Io(error.to_string())
    }
    pub(super) fn raw(file: &std::fs::File) -> HANDLE {
        HANDLE(file.as_raw_handle() as *mut _)
    }
    pub(super) fn cleanup_owned(file: &std::fs::File) -> Result<(), CopyError> {
        #[cfg(test)]
        if COPY_TEST_FAIL_CLEANUP.with(|value| value.replace(false)) {
            return Err(CopyError::Io("injected cleanup failure".into()));
        }
        let info = FILE_DISPOSITION_INFO {
            DeleteFile: BOOLEAN(1),
        };
        unsafe {
            SetFileInformationByHandle(
                raw(file),
                FileDispositionInfo,
                &info as *const _ as *const c_void,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .map_err(io)
        }
    }
    pub(super) fn delete_exact_source(file: &std::fs::File) -> Result<(), CopyError> {
        let info = FILE_DISPOSITION_INFO {
            DeleteFile: BOOLEAN(1),
        };
        unsafe {
            SetFileInformationByHandle(
                raw(file),
                FileDispositionInfo,
                &info as *const _ as *const c_void,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .map_err(io)
        }
    }

    pub(super) fn undo_cross_volume_move(
        receipt: &crate::operations::model::MoveUndoReceipt,
    ) -> Result<(), super::MoveUndoFailure> {
        let proof = receipt.cross_volume_file.as_ref().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false,
            message: "cross-volume Move proof is missing".into(),
        })?;
        if let Some(manifest) = proof.directory_manifest.as_ref() {
            if receipt.object_kind != crate::operations::model::MoveUndoObjectKind::Directory {
                return Err(super::MoveUndoFailure {
                    published_restore: false,
                    message: "directory manifest is attached to a non-directory Move".into(),
                });
            }
            return undo_cross_volume_directory_move(receipt, proof, manifest);
        }
        if receipt.object_kind != crate::operations::model::MoveUndoObjectKind::RegularFile {
            return Err(super::MoveUndoFailure { published_restore: false, message: "cross-volume Move Undo requires a regular file".into() });
        }
        let source_parent = receipt.source.parent().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false, message: "source parent is missing".into(),
        })?;
        let destination_parent = receipt.destination.parent().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false, message: "destination parent is missing".into(),
        })?;
        let mut source_paths = Vec::new();
        let source_route = lock_destination_route_root_first(source_parent, |path| source_paths.push(path.to_path_buf()))
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("source route: {error:?}") })?;
        let mut destination_paths = Vec::new();
        let destination_route = lock_destination_route_root_first(destination_parent, |path| destination_paths.push(path.to_path_buf()))
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination route: {error:?}") })?;
        if parent_route_proof(&source_paths, &source_route).ok().as_ref() != Some(&receipt.source_parent_route)
            || parent_route_proof(&destination_paths, &destination_route).ok().as_ref() != Some(&receipt.destination_parent_route)
        {
            return Err(super::MoveUndoFailure { published_restore: false, message: "a captured parent route changed".into() });
        }
        let source_volume = source_route.last().and_then(|handle| directory_identity(handle).ok()).map(|identity| identity.volume);
        let destination_volume = destination_route.last().and_then(|handle| directory_identity(handle).ok()).map(|identity| identity.volume);
        if source_volume.is_none() || destination_volume.is_none() || source_volume == destination_volume {
            return Err(super::MoveUndoFailure { published_restore: false, message: "parent volumes do not match a cross-volume Move".into() });
        }
        use std::os::windows::ffi::OsStrExt;
        let destination_leaf = receipt.destination.file_name().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false, message: "destination leaf is missing".into(),
        })?.encode_wide().collect::<Vec<_>>();
        let destination_parent_handle = destination_route.last().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false, message: "destination parent route is empty".into(),
        })?;
        match nt_probe_child_no_follow(destination_parent_handle, &destination_leaf)
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination leaf probe: {error:?}") })?
        {
            NtChildProbe::Absent => return Ok(()),
            NtChildProbe::Present(handle) => unsafe {
                CloseHandle(handle).map_err(|error| super::MoveUndoFailure {
                    published_restore: false, message: format!("destination leaf probe close: {error:?}"),
                })?;
            },
        }
        let mut destination_file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&receipt.destination)
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination open: {error:?}") })?;
        let identity = regular_file_identity(&destination_file)
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination identity: {error:?}") })?;
        let signature = leaf_signature(&destination_file)
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination signature: {error:?}") })?;
        if identity.shared() != receipt.object_identity
            || signature != proof.destination_signature
            || !signature.is_reliable_for_cross_volume_move_retry()
        {
            return Err(super::MoveUndoFailure { published_restore: false, message: "destination identity or signature changed".into() });
        }
        let observed_hash = hash_held_file(&destination_file)
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination hash: {error:?}") })?;
        if observed_hash != proof.content_sha256 {
            return Err(super::MoveUndoFailure { published_restore: false, message: "destination content changed".into() });
        }
        use std::io::{Seek, SeekFrom};
        destination_file.seek(SeekFrom::Start(0))
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("destination rewind: {error:?}") })?;
        let original_parent = source_route.last().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false, message: "source parent route is empty".into(),
        })?;
        let mut on_bytes = |_| {};
        let (restored, _, stage_path) = stage_open_regular_file(
            &destination_file,
            &receipt.source,
            &AtomicBool::new(false),
            &mut on_bytes,
            true,
            &mut |_, _| {},
        ).map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("restore staging: {error:?}") })?;
        let stage_identity = regular_file_identity(&restored)
            .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("restore stage identity: {error:?}") })?;
        let publish_result = publish_no_replace_handle_at_parent(raw(&restored), original_parent.0, receipt.source.file_name().ok_or_else(|| super::MoveUndoFailure {
            published_restore: false, message: "source leaf is missing".into(),
        })?);
        let (_published_path, restored) = match publish_result {
            Ok(()) => (receipt.source.clone(), restored),
            Err(crate::operations::rename::RenameError::DestinationExists) => {
                // no-replace の明確な衝突だけ次候補へ進める。stage の同一性も確かめる。
                let stage_leaf = stage_path.file_name().ok_or_else(|| super::MoveUndoFailure {
                    published_restore: false, message: "restore stage leaf is missing after collision".into(),
                })?;
                use std::os::windows::ffi::OsStrExt;
                let stage_units = stage_leaf.encode_wide().collect::<Vec<_>>();
                match nt_probe_child_no_follow(original_parent, &stage_units)
                    .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("restore stage custody probe: {error:?}") })? {
                    NtChildProbe::Present(handle) => {
                        use std::os::windows::io::FromRawHandle;
                        let staged = unsafe { std::fs::File::from_raw_handle(handle.0) };
                        if regular_file_identity(&staged).ok() != Some(stage_identity) {
                            return Err(super::MoveUndoFailure { published_restore: false, message: "restore stage identity changed after collision".into() });
                        }
                    }
                    NtChildProbe::Absent => return Err(super::MoveUndoFailure { published_restore: false, message: "restore stage disappeared after collision".into() }),
                }
                let moved_leaf = receipt.destination.file_name().ok_or_else(|| super::MoveUndoFailure {
                    published_restore: false, message: "moved destination leaf is missing".into(),
                })?;
                let source_leaf = receipt.source.file_name().ok_or_else(|| super::MoveUndoFailure {
                    published_restore: false, message: "source leaf is missing".into(),
                })?;
                let source_units = source_leaf.encode_wide().collect::<Vec<_>>();
                let moved_units = moved_leaf.encode_wide().collect::<Vec<_>>();
                let same_leaf = unsafe {
                    ::windows::Win32::Globalization::CompareStringOrdinal(
                        &source_units,
                        &moved_units,
                        true,
                    ).0 == 2
                };
                let mut published_candidate = None;
                if !same_leaf {
                    match publish_no_replace_handle_at_parent(raw(&restored), original_parent.0, moved_leaf) {
                        Ok(()) => published_candidate = Some(source_parent.join(moved_leaf)),
                        Err(crate::operations::rename::RenameError::DestinationExists) => {
                            match nt_probe_child_no_follow(original_parent, &stage_units)
                                .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("restore stage custody probe after moved-leaf collision: {error:?}") })? {
                                NtChildProbe::Present(handle) => {
                                    use std::os::windows::io::FromRawHandle;
                                    let staged = unsafe { std::fs::File::from_raw_handle(handle.0) };
                                    if regular_file_identity(&staged).ok() != Some(stage_identity) {
                                        return Err(super::MoveUndoFailure { published_restore: false, message: "restore stage identity changed after moved-leaf collision".into() });
                                    }
                                }
                                NtChildProbe::Absent => return Err(super::MoveUndoFailure { published_restore: false, message: "restore stage disappeared after moved-leaf collision".into() }),
                            }
                        }
                        Err(error @ crate::operations::rename::RenameError::MutationUncertain(_)) => return Err(super::uncertain_move_undo_publication_failure(error, "restore moved-leaf publication")),
                        Err(error) => return Err(super::MoveUndoFailure { published_restore: false, message: format!("restore moved-leaf publication: {error:?}; stage retained at {}", stage_path.display()) }),
                    }
                }
                let moved_path = Path::new(moved_leaf);
                let stem = moved_path.file_stem().unwrap_or(moved_leaf).to_string_lossy();
                let extension = moved_path.extension().map(|value| value.to_string_lossy().into_owned());
                for number in 2..1000 {
                    if published_candidate.is_some() { break; }
                    let leaf = match extension.as_deref() {
                        Some(extension) => format!("{stem} ({number}).{extension}"),
                        None => format!("{stem} ({number})"),
                    };
                    match publish_no_replace_handle_at_parent(raw(&restored), original_parent.0, std::ffi::OsStr::new(&leaf)) {
                        Ok(()) => { published_candidate = Some(source_parent.join(&leaf)); break; }
                        Err(crate::operations::rename::RenameError::DestinationExists) => {
                            // 同じstageが未公開のまま残っている場合だけ次候補を試す。
                            match nt_probe_child_no_follow(original_parent, &stage_units)
                                .map_err(|error| super::MoveUndoFailure { published_restore: false, message: format!("restore stage custody probe: {error:?}") })? {
                                NtChildProbe::Present(handle) => {
                                    use std::os::windows::io::FromRawHandle;
                                    let staged = unsafe { std::fs::File::from_raw_handle(handle.0) };
                                    if regular_file_identity(&staged).ok() != Some(stage_identity) {
                                        return Err(super::MoveUndoFailure { published_restore: false, message: "restore stage identity changed between candidate collisions".into() });
                                    }
                                }
                                NtChildProbe::Absent => return Err(super::MoveUndoFailure { published_restore: false, message: "restore stage disappeared between candidate collisions".into() }),
                            }
                        }
                        Err(error @ crate::operations::rename::RenameError::MutationUncertain(_)) => return Err(super::uncertain_move_undo_publication_failure(error, "restore candidate publication")),
                        Err(error) => return Err(super::MoveUndoFailure { published_restore: false, message: format!("restore candidate publication: {error:?}; stage retained at {}", stage_path.display()) }),
                    }
                }
                let candidate_path = published_candidate.ok_or_else(|| super::MoveUndoFailure { published_restore: false, message: format!("bounded restore candidates exhausted; stage retained at {}", stage_path.display()) })?;
                (candidate_path, restored)
            }
            Err(error @ crate::operations::rename::RenameError::MutationUncertain(_)) => return Err(super::uncertain_move_undo_publication_failure(error, "restore publication")),
            Err(error) => return Err(super::MoveUndoFailure { published_restore: false, message: format!("restore publication: {error:?}; stage retained at {}", stage_path.display()) }),
        };
        #[cfg(test)]
        let published_path = _published_path;
        let restored_hash = hash_held_file(&restored)
            .map_err(|error| super::MoveUndoFailure { published_restore: true, message: format!("restored output hash: {error:?}") })?;
        if restored_hash != proof.content_sha256 {
            return Err(super::MoveUndoFailure { published_restore: true, message: "restored output content changed after publication".into() });
        }
        #[cfg(test)]
        if take_cross_volume_move_undo_restored_write_probe_for_test() {
            match std::fs::OpenOptions::new().write(true).open(&published_path) {
                Err(error) if error.raw_os_error() == Some(32) => {}
                Err(error) => return Err(super::MoveUndoFailure { published_restore: true, message: format!("restored output write probe failed for an unexpected reason: {error:?}") }),
                Ok(_) => return Err(super::MoveUndoFailure { published_restore: true, message: "restored output allowed an external writable open before disposition".into() }),
            }
        }
        #[cfg(test)]
        if take_cross_volume_move_undo_destination_delete_failure_for_test() {
            return Err(super::MoveUndoFailure { published_restore: true, message: "injected moved-output disposition failure after restore publication".into() });
        }
        delete_exact_source(&destination_file)
            .map_err(|error| super::MoveUndoFailure { published_restore: true, message: format!("restored source published but moved output deletion failed: {error:?}") })
    }

    fn undo_cross_volume_directory_move(
        receipt: &crate::operations::model::MoveUndoReceipt,
        proof: &crate::operations::model::CrossVolumeMoveUndoProof,
        manifest: &super::super::journal::JournalDirectoryMove,
    ) -> Result<(), super::MoveUndoFailure> {
        use super::super::journal::JournalDirectoryNodeKind;
        let before_publish = |message: String| super::MoveUndoFailure {
            published_restore: false,
            message,
        };
        if manifest.replace.is_some()
            || manifest.cleanup_complete != true
            || manifest.pending.is_some()
            || manifest.root.as_ref().is_none_or(|root| {
                root.pending.is_some()
                    || !root.disposition_recorded
                    || root.source_identity.is_empty()
                    || root.destination_identity.is_empty()
            })
            || manifest.children.iter().enumerate().any(|(ordinal, child)| {
                child.ordinal != ordinal as u32
                    || !child.disposition_recorded
                    || child.node_kind == JournalDirectoryNodeKind::Unknown
                    || child.relative_components.is_empty()
                    || child.source_identity.is_empty()
                    || child.destination_identity.is_empty()
            })
            || manifest.source.to_path().as_ref() != Some(&receipt.source)
            || manifest.destination.to_path().as_ref() != Some(&receipt.destination)
        {
            return Err(before_publish("forward directory manifest is incomplete or mismatched".into()));
        }
        let root = manifest.root.as_ref().expect("validated root");
        let source_parent = receipt.source.parent().ok_or_else(|| before_publish("original parent is missing".into()))?;
        let destination_parent = receipt.destination.parent().ok_or_else(|| before_publish("moved parent is missing".into()))?;
        let mut source_paths = Vec::new();
        let source_route = lock_destination_route_root_first(source_parent, |path| source_paths.push(path.to_path_buf()))
            .map_err(|error| before_publish(format!("original parent route: {error:?}")))?;
        let mut destination_paths = Vec::new();
        let destination_route = lock_destination_route_root_first(destination_parent, |path| destination_paths.push(path.to_path_buf()))
            .map_err(|error| before_publish(format!("moved parent route: {error:?}")))?;
        if parent_route_proof(&source_paths, &source_route).ok().as_ref() != Some(&receipt.source_parent_route)
            || parent_route_proof(&destination_paths, &destination_route).ok().as_ref() != Some(&receipt.destination_parent_route)
        {
            return Err(before_publish("a captured parent route changed".into()));
        }
        let original_parent = source_route.last().ok_or_else(|| before_publish("original parent route is empty".into()))?;
        let moved_parent = destination_route.last().ok_or_else(|| before_publish("moved parent route is empty".into()))?;
        let original_parent_identity = directory_identity(original_parent).map_err(|error| before_publish(format!("original parent identity: {error:?}")))?;
        let moved_parent_identity = directory_identity(moved_parent).map_err(|error| before_publish(format!("moved parent identity: {error:?}")))?;
        let identity_text = |identity: &crate::filesystem_identity::FilesystemIdentity| {
            identity.windows_parts().map(|(volume, index)| format!("{volume}:{:08x}{:08x}", (index >> 32) as u32, index as u32))
        };
        if original_parent_identity.volume == moved_parent_identity.volume
            || Some(root.source_identity.as_str()) != identity_text(&proof.original_source_identity).as_deref()
            || Some(root.destination_identity.as_str()) != identity_text(&receipt.object_identity).as_deref()
        {
            return Err(before_publish("directory identity or volume preflight changed".into()));
        }

        let moved_leaf = receipt.destination.file_name()
            .ok_or_else(|| before_publish("moved directory name is missing".into()))?
            .encode_wide()
            .collect::<Vec<_>>();
        match nt_probe_child_no_follow(moved_parent, &moved_leaf)
            .map_err(|error| before_publish(format!("probe moved directory leaf: {error:?}")))?
        {
            NtChildProbe::Absent => return Ok(()),
            NtChildProbe::Present(handle) => unsafe {
                CloseHandle(handle)
                    .map_err(|error| before_publish(format!("close moved directory leaf probe: {error:?}")))?;
            },
        }

        // The opened tree is the copy source and exact cleanup custody. It
        // deliberately observes current contents; the forward manifest only
        // authorizes disposition of the exact nodes it recorded.
        let expected_moved_parent = identity_text(&receipt.destination_parent_route.last()
            .ok_or_else(|| before_publish("moved parent proof is missing".into()))?.identity)
            .ok_or_else(|| before_publish("moved parent identity is not Windows-backed".into()))?;
        let mut moved_tree = preflight_pinned_source_tree_for_cross_volume_undo(
            &receipt.destination,
            &AtomicBool::new(false),
            &expected_moved_parent,
            &root.destination_identity,
        ).map_err(|error| before_publish(format!("moved tree preflight: {error:?}")))?;
        if moved_tree.root_identity != root.destination_identity {
            return Err(before_publish("moved directory root identity changed".into()));
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| before_publish(format!("generate restore staging name: {error}")))?;
        let stage_path = source_parent.join(format!(".gpui-filer-directory-undo-{:032x}", u128::from_be_bytes(random)));
        let cancelled = AtomicBool::new(false);
        let staging = create_attempt_staging_root(&stage_path)
            .map_err(|error| before_publish(format!("create restore stage: {error:?}")))?;
        let mut staged = stage_pinned_source_tree(&mut moved_tree, staging, &cancelled)
            .map_err(|error| before_publish(format!("stage current moved tree: {error:?}")))?;
        let original_name = receipt.source.file_name().and_then(|name| name.to_str())
            .ok_or_else(|| before_publish("original directory name is not representable".into()))?;
        let mut candidate = receipt.source.clone();
        let mut suffix = 2u16;
        let restored = loop {
            match publish_and_verify_staged_tree_for_undo(staged, &candidate, &cancelled) {
                Ok(restored) => break restored,
                Err(UndoTreePublishFailure::RetryableCollision(held_stage)) => {
                    if suffix > 999 {
                        let cleanup = clean_staged_failure_from_facts_error(held_stage, CopyError::DestinationExists);
                        return Err(before_publish(format!("all Keep Both directory names from (2) through (999) are occupied; staging cleanup: {cleanup:?}")));
                    }
                    candidate = source_parent.join(format!("{original_name} ({suffix})"));
                    suffix += 1;
                    staged = held_stage;
                }
                Err(UndoTreePublishFailure::BeforePublish(error)) => return Err(before_publish(format!("restore publication refused: {error:?}"))),
                Err(UndoTreePublishFailure::AfterPublish { error }) => {
                    return Err(super::MoveUndoFailure { published_restore: true, message: format!("restore root was published but verification failed: {error:?}") });
                }
            }
        };
        let _restored_root_identity = directory_identity(&restored.root)
            .map_err(|error| super::MoveUndoFailure { published_restore: true, message: format!("restored root identity: {error:?}") })?;

        #[cfg(test)]
        if take_cross_volume_move_undo_destination_delete_failure_for_test() {
            return Err(super::MoveUndoFailure { published_restore: true, message: "injected moved-directory cleanup refusal after restore publication".into() });
        }
        let mut order: Vec<_> = manifest.children.iter().collect();
        order.sort_by(|left, right| right.relative_components.len().cmp(&left.relative_components.len()).then_with(|| left.ordinal.cmp(&right.ordinal)));
        for child in order {
            let Some(entry) = moved_tree.entries.iter().find(|entry| {
                entry.relative_components == child.relative_components && entry.node_kind == child.node_kind
            }) else { continue };
            if entry.source_identity != child.destination_identity {
                continue;
            }
            let Some(Some(handle)) = moved_tree._handles.get(entry.handle_index) else { continue };
            let identity_matches = match handle {
                PinnedSourceHandle::Directory(directory) => directory_identity_text(directory).is_ok_and(|actual| actual == child.destination_identity),
                PinnedSourceHandle::File(file) => regular_file_identity(file).is_ok_and(|actual| {
                    format!("{}:{:08x}{:08x}", actual.volume, actual.index_high, actual.index_low) == child.destination_identity
                }),
            };
            if !identity_matches {
                return Err(super::MoveUndoFailure { published_restore: true, message: format!("exact moved child identity changed at ordinal {}", child.ordinal) });
            }
            if let Err(error) = set_handle_delete_disposition(handle) {
                return Err(super::MoveUndoFailure { published_restore: true, message: format!("restored tree published; exact moved child cleanup refused at ordinal {}: {error:?}", child.ordinal) });
            }
            moved_tree._handles[entry.handle_index] = None;
        }
        let root_handle = moved_tree._handles.get(moved_tree.root_handle_index).and_then(Option::as_ref)
            .ok_or_else(|| super::MoveUndoFailure { published_restore: true, message: "moved root custody was lost after restore publication".into() })?;
        let PinnedSourceHandle::Directory(root_handle) = root_handle else {
            return Err(super::MoveUndoFailure { published_restore: true, message: "moved root custody has the wrong kind".into() });
        };
        if directory_identity_text(root_handle).ok().as_deref() != Some(root.destination_identity.as_str())
            || !enumerate_directory_handle(root_handle, 1, 2).is_ok_and(|children| children.is_empty())
        {
            return Err(super::MoveUndoFailure { published_restore: true, message: "restored tree published; moved root remains nonempty or changed and was preserved".into() });
        }
        set_raw_handle_delete_disposition(root_handle.0)
            .map_err(|error| super::MoveUndoFailure { published_restore: true, message: format!("restored tree published; exact moved root disposition refused: {error:?}") })?;
        moved_tree._handles[moved_tree.root_handle_index] = None;
        Ok(())
    }

    pub(super) fn cleanup_copy_stage_handle(
        lease: super::super::runtime::CopyStageCleanupLease,
        cancelled: &AtomicBool,
    ) -> Result<bool, CopyError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        if regular_file_identity(&lease.handle)?.shared() != lease.identity {
            return Err(CopyError::GuardRejected(
                "retained Copy stage handle identity changed".into(),
            ));
        }
        let parent = lease.path.parent().ok_or(CopyError::Unsupported)?;
        let route = lock_destination_route_root_first(parent, |_| {})?;
        let route_identity = directory_identity(route.last().ok_or(CopyError::Unsupported)?)?;
        let actual_parent = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            route_identity.volume,
            (u64::from(route_identity.index_high) << 32) | u64::from(route_identity.index_low),
        );
        if actual_parent != lease.parent_identity {
            return Err(CopyError::GuardRejected(
                "retained Copy stage parent route changed".into(),
            ));
        }
        // A read-only no-follow lookup confirms that the retained leaf still names
        // this same regular file before disposition; deletion itself uses only the
        // continuously held original stage handle below.
        let current = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&lease.path)
            .map_err(io)?;
        if regular_file_identity(&current)?.shared() != lease.identity {
            return Err(CopyError::GuardRejected(
                "retained Copy stage path no longer names the owned stage".into(),
            ));
        }
        drop(current);
        if cancelled.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        #[cfg(test)]
        let disposition = {
            let info = FILE_DISPOSITION_INFO {
                DeleteFile: BOOLEAN(1),
            };
            unsafe {
                SetFileInformationByHandle(
                    raw(&lease.handle),
                    FileDispositionInfo,
                    &info as *const _ as *const c_void,
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            }
        };
        #[cfg(test)]
        if let Some(observer) = lease.native_disposition_observer.as_ref() {
            let observed = disposition
                .as_ref()
                .map(|_| ())
                .map_err(|error| error.code().0 as u32);
            let _ = observer.send_blocking(observed);
        }
        #[cfg(test)]
        if disposition.is_err() {
            return Ok(false);
        }
        #[cfg(not(test))]
        if delete_exact_source(&lease.handle).is_err() {
            return Ok(false);
        }
        let path = lease.path.clone();
        drop(lease);
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            _ => Ok(false),
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct RegularFileIdentity {
        pub(super) volume: u32,
        pub(super) index_high: u32,
        pub(super) index_low: u32,
    }
    impl RegularFileIdentity {
        fn shared(self) -> crate::filesystem_identity::FilesystemIdentity {
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                self.volume,
                (u64::from(self.index_high) << 32) | u64::from(self.index_low),
            )
        }
    }
    pub(super) fn regular_file_identity(
        file: &std::fs::File,
    ) -> Result<RegularFileIdentity, CopyError> {
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(raw(file), &mut info).map_err(io)?;
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(CopyError::Unsupported);
        }
        Ok(RegularFileIdentity {
            volume: info.dwVolumeSerialNumber,
            index_high: info.nFileIndexHigh,
            index_low: info.nFileIndexLow,
        })
    }

    pub(super) fn inspect_guarded_move_no_effect(
        proof: &super::super::model::GuardedMoveNoEffectProof,
    ) -> (
        bool,
        bool,
        crate::operations::model::GuardedMoveDestinationState,
    ) {
        let source_matches = prepare_regular_file_source(&proof.source)
            .ok()
            .and_then(|source| source.shared_identity())
            .is_some_and(|identity| identity == proof.source_identity);
        let parent_matches = proof.destination.parent().is_some_and(|parent| {
            lock_non_reparse_ancestors(parent)
                .ok()
                .and_then(|locks| {
                    locks
                        .first()
                        .map(|lock| directory_identity(lock).ok())
                        .flatten()
                })
                .is_some_and(|identity| {
                    crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                        identity.volume,
                        (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
                    ) == proof.destination_parent_identity
                })
        });
        let destination = match std::fs::symlink_metadata(&proof.destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::operations::model::GuardedMoveDestinationState::Absent
            }
            Ok(_) => crate::operations::model::GuardedMoveDestinationState::Present,
            Err(_) => crate::operations::model::GuardedMoveDestinationState::Indeterminate,
        };
        (source_matches, parent_matches, destination)
    }

    pub(super) fn inspect_cross_volume_move_no_effect(
        proof: &crate::operations::model::CrossVolumeMoveNoEffectProof,
    ) -> (
        bool,
        bool,
        crate::operations::model::GuardedMoveDestinationState,
    ) {
        let source_matches = prepare_regular_file_source(&proof.source).ok().is_some_and(|source| {
            source.shared_identity().as_ref() == Some(&proof.source_identity)
                && leaf_signature(&source.file).is_ok_and(|signature| signature == proof.source_signature)
        });
        let parent_matches = proof.destination.parent().is_some_and(|parent| {
            lock_non_reparse_ancestors(parent).ok().is_some_and(|locks| locks.first().is_some_and(|handle| {
                directory_identity(handle).is_ok_and(|identity| crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                    identity.volume,
                    (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
                ) == proof.destination_parent_identity)
            }))
        });
        let destination = match std::fs::symlink_metadata(&proof.destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => crate::operations::model::GuardedMoveDestinationState::Absent,
            Ok(_) => crate::operations::model::GuardedMoveDestinationState::Present,
            Err(_) => crate::operations::model::GuardedMoveDestinationState::Indeterminate,
        };
        (source_matches, parent_matches, destination)
    }

    pub(super) fn inspect_published_move(
        proof: &crate::operations::model::PublishedMoveSourceRetainedProof,
    ) -> (
        crate::operations::model::PublishedMoveInspectionState,
        crate::operations::model::PublishedMoveInspectionState,
        crate::operations::model::PublishedMoveInspectionState,
    ) {
        use crate::operations::model::PublishedMoveInspectionState as State;
        let source = match std::fs::symlink_metadata(&proof.source) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
            Err(_) => State::Indeterminate,
            Ok(metadata) if !metadata.file_type().is_file() => State::Changed,
            Ok(_) => match prepare_regular_file_source(&proof.source) {
                Ok(file) => match (file.shared_identity(), leaf_signature(&file.file)) {
                    (Some(identity), Ok(signature)) if identity == proof.source_identity && signature == proof.source_signature => State::Matches,
                    (Some(_), Ok(_)) => State::Changed,
                    _ => State::Indeterminate,
                },
                Err(_) => State::Indeterminate,
            },
        };
        let parent = match proof.destination.parent() {
            Some(path) => match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
                Err(_) => State::Indeterminate,
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    match lock_non_reparse_ancestors(path).ok().and_then(|locks| locks.first().and_then(|lock| directory_identity(lock).ok())) {
                        Some(actual) if crate::filesystem_identity::FilesystemIdentity::from_windows_parts(actual.volume, (u64::from(actual.index_high) << 32) | u64::from(actual.index_low)) == proof.destination_parent_identity => State::Matches,
                        Some(_) => State::Changed,
                        None => State::Indeterminate,
                    }
                }
                Ok(_) => State::Changed,
            },
            None => State::Indeterminate,
        };
        let destination = match std::fs::symlink_metadata(&proof.destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
            Err(_) => State::Indeterminate,
            Ok(metadata) if !metadata.file_type().is_file() => State::Changed,
            Ok(_) => match prepare_regular_file_source(&proof.destination) {
                Ok(file) => match (file.shared_identity(), leaf_signature(&file.file)) {
                    (Some(identity), Ok(signature)) if identity == proof.destination_identity && signature == proof.destination_signature => State::Matches,
                    (Some(_), Ok(_)) => State::Changed,
                    (None, _) | (_, Err(_)) => State::Indeterminate,
                },
                Err(_) => State::Indeterminate,
            },
        };
        (source, parent, destination)
    }

    pub(super) fn inspect_directory_root_identity(path_units: &[u16], expected: Option<&str>) -> crate::operations::model::CopyInspectionState {
        use crate::operations::model::CopyInspectionState as State;
        use ::windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, GetFileInformationByHandle, OPEN_EXISTING};
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_wide(path_units));
        let Some(parent) = path.parent() else { return State::Indeterminate };
        let _parent_route = match lock_copy_source_ancestry_root_first(parent) {
            Ok(route) => route,
            Err(_) => return State::Indeterminate,
        };
        let wide_path = wide(&path);
        let handle = unsafe { CreateFileW(PCWSTR(wide_path.as_ptr()), FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ, None,
            OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, None) };
        let handle = match handle {
            Ok(handle) => DirectoryLock(handle),
            // The parent route is already pinned and validated, so this Win32
            // error can only mean that the named leaf is absent.
            Err(error) if error.code().0 as u32 == 0x8007_0002 => return State::Absent,
            Err(_) => return State::Indeterminate,
        };
        let mut info = Default::default();
        if unsafe { GetFileInformationByHandle(handle.0, &mut info) }.is_err() { return State::Indeterminate; }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            return State::Changed;
        }
        let identity = format!("{}:{:08x}{:08x}", info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow);
        if expected == Some(identity.as_str()) { State::Matches } else { State::Changed }
    }

    pub(super) fn inspect_copy_failure(
        provenance: &crate::operations::model::CopyFailureProvenance,
    ) -> (
        crate::operations::model::CopyInspectionState,
        crate::operations::model::CopyInspectionState,
        crate::operations::model::CopyInspectionState,
        crate::operations::model::CopyInspectionState,
    ) {
        use crate::operations::model::CopyInspectionState as State;
        use ::windows::Win32::{
            Foundation::GENERIC_READ,
            Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
        };
        use std::os::windows::fs::OpenOptionsExt;
        let source = match (&provenance.source_identity, provenance.source_signature) {
            (Some(expected_identity), Some(expected_signature)) => {
                match std::fs::symlink_metadata(&provenance.source) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
                    Err(_) => State::Indeterminate,
                    Ok(metadata) if !metadata.file_type().is_file() => State::Changed,
                    Ok(_) => match prepare_regular_file_source(&provenance.source) {
                        Ok(opened) => {
                            match (opened.shared_identity(), leaf_signature(&opened.file)) {
                                (Some(identity), Ok(signature))
                                    if identity != *expected_identity
                                        || signature != expected_signature =>
                                {
                                    State::Changed
                                }
                                (Some(_), Ok(_)) => State::Matches,
                                _ => State::Indeterminate,
                            }
                        }
                        Err(_) => State::Indeterminate,
                    },
                }
            }
            _ => State::Indeterminate,
        };
        let parent = match (
            &provenance.destination_parent_identity,
            provenance.destination.parent(),
        ) {
            (Some(expected), Some(path)) => match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
                Err(_) => State::Indeterminate,
                Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                    State::Changed
                }
                Ok(_) => match lock_non_reparse_ancestors(path)
                    .ok()
                    .and_then(|locks| locks.first().map(directory_identity).transpose().ok())
                    .flatten()
                {
                    Some(actual) => {
                        let actual =
                            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                                actual.volume,
                                (u64::from(actual.index_high) << 32) | u64::from(actual.index_low),
                            );
                        if actual == *expected {
                            State::Matches
                        } else {
                            State::Changed
                        }
                    }
                    None => State::Indeterminate,
                },
            },
            _ => State::Indeterminate,
        };
        let current_file =
            |path: &Path, expected: Option<&crate::filesystem_identity::FilesystemIdentity>| {
                match std::fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
                    Err(_) => State::Indeterminate,
                    Ok(metadata) if !metadata.file_type().is_file() => State::Changed,
                    Ok(_) => match prepare_regular_file_source(path) {
                        Ok(opened) => match (expected, opened.shared_identity()) {
                            (Some(expected), Some(actual)) if actual == *expected => State::Matches,
                            (Some(_), Some(_)) => State::Changed,
                            _ => State::Indeterminate,
                        },
                        Err(_) => State::Indeterminate,
                    },
                }
            };
        let destination = current_file(
            &provenance.destination,
            provenance
                .stage
                .as_ref()
                .and_then(|stage| stage.identity.as_ref()),
        );
        let stage = provenance
            .stage
            .as_ref()
            .map_or(State::Indeterminate, |stage| {
                match std::fs::symlink_metadata(&stage.path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Absent,
                    Err(_) => State::Indeterminate,
                    Ok(metadata) if !metadata.file_type().is_file() => State::Changed,
                    Ok(_) => {
                        // Original custody denies write/delete access and therefore
                        // cannot coexist with prepare_regular_file_source's restrictive
                        // sharing mode. This read-only identity probe shares all modes;
                        // its identity result is still backed by a no-follow file handle.
                        let opened = std::fs::OpenOptions::new()
                            .read(true)
                            .access_mode(GENERIC_READ.0)
                            .share_mode(
                                FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0,
                            )
                            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
                            .open(&stage.path);
                        match opened {
                            Ok(file) => {
                                match (stage.identity.as_ref(), regular_file_identity(&file)) {
                                    (Some(expected), Ok(actual))
                                        if actual.shared() == *expected =>
                                    {
                                        State::Matches
                                    }
                                    (Some(_), Ok(_)) => State::Changed,
                                    _ => State::Indeterminate,
                                }
                            }
                            Err(_) => State::Indeterminate,
                        }
                    }
                }
            });
        (source, parent, destination, stage)
    }

    #[cfg(test)]
    pub(super) fn guarded_move_inspection_fixture_identities(
        source: &Path,
        destination_parent: &Path,
    ) -> Option<(
        crate::filesystem_identity::FilesystemIdentity,
        crate::filesystem_identity::FilesystemIdentity,
    )> {
        let source = prepare_regular_file_source(source)
            .ok()?
            .shared_identity()?;
        let parents = lock_non_reparse_ancestors(destination_parent).ok()?;
        let identity = directory_identity(parents.first()?).ok()?;
        let destination_parent = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            identity.volume,
            (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
        );
        Some((source, destination_parent))
    }
    pub(super) fn leaf_signature(
        file: &std::fs::File,
    ) -> Result<crate::operations::model::SourceCleanupLeafSignature, CopyError> {
        // file IDだけでは同じfileへのhardlinkを見分けられないため、link数とChangeTimeも照合する。
        let mut standard = FILE_STANDARD_INFO::default();
        let mut basic = FILE_BASIC_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                raw(file),
                FileStandardInfo,
                &mut standard as *mut _ as *mut c_void,
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
            .map_err(io)?;
            GetFileInformationByHandleEx(
                raw(file),
                FileBasicInfo,
                &mut basic as *mut _ as *mut c_void,
                size_of::<FILE_BASIC_INFO>() as u32,
            )
            .map_err(io)?;
        }
        if standard.NumberOfLinks == 0 {
            return Err(CopyError::Unsupported);
        }
        Ok(crate::operations::model::SourceCleanupLeafSignature {
            number_of_links: standard.NumberOfLinks,
            change_time: basic.ChangeTime,
        })
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct DirectoryIdentity {
        pub(super) volume: u32,
        pub(super) index_high: u32,
        pub(super) index_low: u32,
    }
    fn directory_identity(directory: &DirectoryLock) -> Result<DirectoryIdentity, CopyError> {
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(directory.0, &mut info).map_err(io)?;
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(CopyError::Unsupported);
        }
        Ok(DirectoryIdentity {
            volume: info.dwVolumeSerialNumber,
            index_high: info.nFileIndexHigh,
            index_low: info.nFileIndexLow,
        })
    }
    pub(super) fn publish_no_replace(
        file: &std::fs::File,
        destination: &Path,
    ) -> Result<(), CopyError> {
        publish_no_replace_handle(raw(file), destination)
    }

    pub(super) fn create_virtual_stage(path: &Path) -> Result<std::fs::File, CopyError> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0)
            .open(path)
            .map_err(io)
    }
    pub(super) fn publish_no_replace_handle(
        handle: HANDLE,
        destination: &Path,
    ) -> Result<(), CopyError> {
        let name = wide(destination);
        let bytes = name.len().saturating_sub(1) * size_of::<u16>();
        let size = offset_of!(FILE_RENAME_INFO, FileName) + name.len() * size_of::<u16>();
        let mut storage = vec![0_u64; size.div_ceil(size_of::<u64>())];
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = BOOLEAN(0);
            (*info).RootDirectory = HANDLE::default();
            (*info).FileNameLength = bytes as u32;
            std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
            SetFileInformationByHandle(handle, FileRenameInfo, info.cast(), size as u32).map_err(
                |error| {
                    match error.code().0 as u32 {
                        0x8007_0050 | 0x8007_00b7 => CopyError::DestinationExists,
                        0x8007_0005 => CopyError::AccessDenied,
                        _ => io(error),
                    }
                },
            )
        }
    }

    pub(super) fn publish_no_replace_handle_at_parent(
        handle: HANDLE,
        parent: HANDLE,
        leaf: &std::ffi::OsStr,
    ) -> Result<(), crate::operations::rename::RenameError> {
        crate::operations::rename::rename_relative_handle_no_replace(
            handle.0 as *mut std::ffi::c_void,
            parent.0 as *mut std::ffi::c_void,
            leaf,
        )
    }
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn arm_copy_directory_postpublish_fault_for_shell_test() {
    windows::arm_copy_directory_foreign_child_for_test();
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn arm_copy_replace_backup_cleanup_failure_for_shell_test(
    token: AttemptToken,
    item: ItemId,
) {
    windows::arm_copy_replace_backup_cleanup_failure_for_test(token, item);
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn arm_copy_replace_backup_child_cleanup_failure_for_shell_test(
    token: AttemptToken,
    item: ItemId,
    intent: super::journal::JournalCopyReplaceChildIntent,
    durable_ack_gate: std::sync::mpsc::Receiver<()>,
) {
    windows::arm_copy_replace_backup_child_cleanup_failure_for_test(token, item, intent, durable_ack_gate);
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn probe_copy_source_directory_lock_for_shell_test(path: &Path) -> Result<(), String> {
    windows::probe_copy_source_directory_lock_for_test(path)
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn fail_next_copy_directory_stage_root_identity_for_test() {
    windows::fail_next_copy_directory_stage_root_identity_for_test();
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn close_directory_copy_undo_injected_handle_after_terminal_for_test(
    custody: &DirectoryCopyUndoCustodySlot,
) -> bool {
    windows::extract_cleanup::close_captured_uncertain_handle_after_terminal_for_test(
        custody.injected_close_handle.as_ref(),
    )
}

#[cfg(all(test, windows))]
mod guarded_move_inspection_tests {
    use super::super::model::{
        AttemptToken, GuardedMoveDestinationState, GuardedMoveInspectionCandidate,
        GuardedMoveInspectionResult, GuardedMoveNoEffectProof, InspectionTicket, ItemId,
    };

    #[test]
    fn inspection_revalidates_replacements_and_destination_creation_without_mutation() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source_parent = root.path().join("source-parent");
        let destination_parent = root.path().join("destination-parent");
        std::fs::create_dir(&source_parent).unwrap();
        std::fs::create_dir(&destination_parent).unwrap();
        let source = source_parent.join("source.txt");
        let destination = destination_parent.join("source.txt");
        std::fs::write(&source, b"original").unwrap();
        let (source_identity, destination_parent_identity) =
            super::windows::guarded_move_inspection_fixture_identities(
                &source,
                &destination_parent,
            )
            .unwrap();
        let proof = GuardedMoveNoEffectProof {
            item: ItemId(0),
            source: source.clone(),
            destination: destination.clone(),
            source_identity,
            destination_parent_identity,
        };
        let make_work = || GuardedMoveInspectionResult {
            ticket: InspectionTicket::new(1),
            attempt: AttemptToken::fixture(),
            candidates: vec![GuardedMoveInspectionCandidate {
                proof: proof.clone(),
                source_matches: false,
                destination_parent_matches: false,
                destination: GuardedMoveDestinationState::Indeterminate,
            }],
        };
        let inspect = |work| {
            super::inspect_guarded_move_candidates(work)
                .candidates
                .remove(0)
        };
        let original = inspect(make_work());
        assert!(
            original.source_matches
                && original.destination_parent_matches
                && original.destination == GuardedMoveDestinationState::Absent
        );
        assert!(
            source.exists() && !destination.exists(),
            "inspection must not mutate either path"
        );

        std::fs::rename(&source, source.with_extension("held")).unwrap();
        std::fs::write(&source, b"replacement").unwrap();
        let replaced_source = inspect(make_work());
        assert!(
            !replaced_source.source_matches
                && replaced_source.destination_parent_matches
                && replaced_source.destination == GuardedMoveDestinationState::Absent
        );

        std::fs::rename(
            &destination_parent,
            destination_parent.with_extension("held"),
        )
        .unwrap();
        std::fs::create_dir(&destination_parent).unwrap();
        let replaced_parent = inspect(make_work());
        assert!(!replaced_parent.destination_parent_matches);
        std::fs::write(&destination, b"collision").unwrap();
        let collision = inspect(make_work());
        assert_eq!(
            collision.destination,
            GuardedMoveDestinationState::Present,
            "a newly created destination is a conflict"
        );
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"collision",
            "inspection preserves external destination"
        );
    }
}

#[cfg(all(test, windows))]
pub(crate) fn fail_next_cross_volume_move_stage_create_for_test() {
    windows::fail_next_cross_volume_move_stage_create_for_test();
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) fn fail_cross_volume_move_source_disposition_for_path_for_test(path: &Path) {
    windows::fail_cross_volume_move_source_disposition_for_path_for_test(path);
}

#[cfg(all(test, windows))]
pub(crate) fn with_directory_move_child_disposition_failure_for_test<R>(
    token: AttemptToken,
    item: u32,
    relative_components: Vec<Vec<u16>>,
    run: impl FnOnce() -> R,
) -> (R, bool) {
    windows::with_directory_move_child_disposition_failure_for_test(
        token,
        item,
        relative_components,
        run,
    )
}

#[cfg(all(test, windows))]
pub(crate) fn fail_next_cross_volume_move_undo_destination_delete_for_test() {
    windows::fail_next_cross_volume_move_undo_destination_delete_for_test();
}

#[cfg(all(test, windows))]
pub(crate) fn fail_cross_volume_move_undo_destination_delete_after_successes_for_test(successes: usize) {
    windows::fail_cross_volume_move_undo_destination_delete_after_successes_for_test(successes);
}

mod seven_zip_create;
#[cfg(test)]
mod tests;
