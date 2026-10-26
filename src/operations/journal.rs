//! Crash journal for operation attempts.
//!
//! This deliberately records uncertainty rather than pretending it can make a
//! filesystem mutation exactly-once.  A record that reached Running but has no
//! durable terminal fact becomes RecoveryRequired on the next launch; callers
//! may inspect/discard it, but this module never replays it.

use super::model::{AttemptSnapshot, JobSnapshot, OperationKind};
use crate::session_store::windows::{SessionFile, SessionPaths};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod seven_zip_create;
pub use seven_zip_create::*;
mod seven_zip_handle_create;
pub use seven_zip_handle_create::*;
mod seven_zip_extract;
pub use seven_zip_extract::*;
mod seven_zip_extract_batch;
pub use seven_zip_extract_batch::*;

#[cfg(test)]
pub(crate) fn seven_zip_create_test_fixture() -> JournalSevenZipCreate {
    seven_zip_create::tests::fixture()
}

#[cfg(test)]
pub(crate) fn seven_zip_handle_create_test_fixture() -> JournalSevenZipHandleCreate {
    seven_zip_handle_create::fixture()
}

#[cfg(test)]
pub(crate) fn seven_zip_extract_test_fixture() -> JournalSevenZipExtractLedger {
    seven_zip_extract::test_fixture()
}

const SCHEMA: u32 = 9;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIRECTORY_ATTEMPT_BYTES: usize = 3 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 1_024;
const MAX_DIRECTORY_DEPTH: usize = 128;
const MAX_DIRECTORY_RELATIVE_NAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalState {
    Queued,
    Preparing,
    Running,
    WaitingConflict,
    CancelRequested,
    /// A Replace backup may exist while the terminal report is partial or
    /// unknown. It is intentionally never replayed automatically.
    RecoveryRequired,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalKind {
    Copy,
    Move,
    CreateSymbolicLink,
    CreateJunction,
    CreateFolder,
    CreateFile,
    CreateShortcut,
    CreateZip,
    Create7z,
    ExtractZip,
    Extract7z,
    Trash,
    Rename,
    BatchRename,
    BatchRenameUndo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalTerminalOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalDirectoryNodeKind {
    File,
    Directory,
    #[default]
    Unknown,
}

/// A lossless Windows path representation.  The journal never uses display
/// text as an I/O target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalPath {
    pub units: Vec<u16>,
}

impl JournalPath {
    pub fn from_path(path: &std::path::Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self {
                units: path.as_os_str().encode_wide().collect(),
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                units: path.to_string_lossy().encode_utf16().collect(),
            }
        }
    }

    pub(super) fn to_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            Some(std::path::PathBuf::from(std::ffi::OsString::from_wide(
                &self.units,
            )))
        }
        #[cfg(not(windows))]
        {
            String::from_utf16(&self.units)
                .ok()
                .map(std::path::PathBuf::from)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalAttempt {
    pub key: String,
    pub kind: JournalKind,
    #[serde(default)]
    pub created_folder_path: Option<JournalPath>,
    #[serde(default)]
    pub created_archive_path: Option<JournalPath>,
    /// Exact ordered ZIP source/output facts. Recovery inspects this ledger only.
    #[serde(default)]
    pub zip_create: Option<JournalZipCreate>,
    /// Separate, inspection-only workspace ledger for 7z creation.
    #[serde(default)]
    pub seven_zip_create: Option<JournalSevenZipCreate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_zip_create_handles: Option<JournalSevenZipHandleCreate>,
    /// Extraction-specific, inspection-only staging and publication facts.
    #[serde(default)]
    pub zip_extract: Option<JournalZipExtract>,
    /// Item-keyed extraction ledgers for multi-archive attempts. Legacy singleton records remain in `zip_extract`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zip_extract_items: Vec<JournalZipExtractItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zip_extract_receipts: Vec<JournalZipExtractReceipt>,
    /// Whole-batch worker registration and ordered item custody for 7z extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_zip_extract_batch: Option<JournalSevenZipExtractBatch>,
    /// Terminal result facts remain separate from worker barrier sequencing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seven_zip_extract_receipts: Vec<JournalSevenZipExtractTerminalReceipt>,
    /// Safe descriptor facts for inspection; the process-local COM token is never persisted.
    #[serde(default)]
    pub virtual_source_name: Option<String>,
    #[serde(default)]
    pub virtual_source_size: Option<u64>,
    #[serde(default)]
    pub virtual_sources: Vec<JournalVirtualSource>,
    /// descriptor順の1桁status code。0 pending、1 completed、2 skipped、3 failed、
    /// 4 cancelled-before-start、5 partial、6 unknown。terminal時も1件1桁を保ち、
    /// running記録よりjournalが増えないようにする。
    #[serde(default)]
    pub virtual_item_status: Vec<u8>,
    pub state: JournalState,
    pub sources: Vec<JournalPath>,
    pub destination: Option<JournalPath>,
    #[serde(default)]
    pub batch_rename_plan: Vec<JournalBatchRenameTarget>,
    #[serde(default)]
    pub batch_rename_completed: Vec<JournalBatchRenameReceipt>,
    /// Written and fsynced before a Replace worker displaces the existing
    /// destination. Recovery never replays this intent; it merely preserves
    /// the exact owned backup path for inspection.
    #[serde(default)]
    pub replace_backup: Option<JournalReplaceBackup>,
    #[serde(default)]
    pub cross_volume_move_replace: Option<JournalCrossVolumeMoveReplace>,
    /// Inspection ledger for same-volume ordinary-directory Move Replace.
    /// It stays separate from Copy staging and cross-volume source deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_move_replace_rename: Option<JournalDirectoryMoveReplaceRename>,
    /// Exact staged case-only rename intent. It is retained for RecoveryRequired
    /// records and is never replayed or rolled back during startup.
    #[serde(default)]
    pub rename_stage: Option<JournalRenameStage>,
    /// Bounded per-item directory Move manifests and per-child cleanup ledgers.
    /// Unresolved intents are inspection data only and are never replayed.
    #[serde(default)]
    pub directory_moves: Vec<JournalDirectoryMove>,
    #[serde(default)]
    pub copy_replace_cleanups: Vec<JournalCopyReplaceCleanup>,
    /// Inspection-only commitment for uncertain ordinary directory Copy publication.
    /// This is not a per-entry manifest and grants no mutation authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_directory_publications: Vec<JournalCopyDirectoryPublicationEvidence>,
    /// Historical evidence for one retained ordinary directory Copy stage.
    /// This is inspection data only and is captured with the first terminal snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_directory_stage: Option<JournalCopyDirectoryStageEvidence>,
    /// A terminal report is factual but does not by itself prove every I/O
    /// receipt made it to disk before a process crash.
    pub receipts_recorded: bool,
    #[serde(default)]
    pub terminal_outcome: Option<JournalTerminalOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalCopyDirectoryStageEvidence {
    pub item: u32,
    pub source: JournalPath,
    pub destination: JournalPath,
    pub evidence: super::model::CopyDirectoryStageEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalCopyDirectoryPublicationPhase {
    RenameOutcomeUncertain,
    PostPublishCancelled,
    PublishedRootOpenFailed,
    PublishedRootIdentityFailed,
    PublishedInventoryFailed,
    PublishedInventoryMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalCopyDirectoryInventoryKind {
    CanonicalTreeSha256NoPerEntryRecovery,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCopyDirectoryPublicationEvidence {
    pub item: u32,
    pub stage_path: JournalPath,
    pub destination_path: JournalPath,
    pub expected_root_identity: String,
    pub expected_node_count: u64,
    pub inventory_kind: JournalCopyDirectoryInventoryKind,
    pub expected_inventory_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_root_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_node_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_inventory_sha256: Option<String>,
    pub phase: JournalCopyDirectoryPublicationPhase,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalVirtualSource {
    pub name: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryMove {
    pub item: u32,
    pub source: JournalPath,
    pub destination: JournalPath,
    pub children: Vec<JournalDirectoryChild>,
    pub pending: Option<JournalDirectoryChildIntent>,
    /// Legacy records may lack root facts; they remain unresolved and cannot complete.
    #[serde(default)]
    pub root: Option<JournalDirectoryRoot>,
    #[serde(default)]
    pub cleanup_complete: bool,
    /// Move Replace extends this source cleanup record with the displaced
    /// destination manifest. Copy Replace has a separate Copy-owned ledger.
    #[serde(default)]
    pub replace: Option<JournalDirectoryMoveReplace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryMoveReplace {
    pub stage: JournalPath,
    pub backup: JournalPath,
    pub stage_identity: String,
    pub approved_target_identity: String,
    pub destination_parent_identity: String,
    pub backup_root_identity: String,
    pub backup_children: Vec<JournalDirectoryMoveReplaceChild>,
    #[serde(default)]
    pub backup_pending: Option<JournalDirectoryMoveReplaceChildIntent>,
    #[serde(default)]
    pub backup_root_disposition_recorded: bool,
    #[serde(default)]
    pub backup_cleanup_complete: bool,
    pub phase: DirectoryMoveReplacePhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryMoveReplacePhase {
    Staged,
    DisplaceIntent,
    TargetDisplaced,
    PublishIntent,
    DestinationPublished,
    SourceCleanup,
    SourceComplete,
    BackupCleanup,
    BackupComplete,
    Complete,
    StageDiscarded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryMoveReplaceChild {
    pub ordinal: u32,
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
    pub disposition_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryMoveReplaceChildIntent {
    pub ordinal: u32,
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
}

/// Durable facts for direct-rename same-volume directory Move Replace.
/// Intent phases with missing receipts retain an explicit uncertain location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalDirectoryMoveReplaceRename {
    pub item: u32,
    pub source: JournalPath,
    pub destination: JournalPath,
    pub backup: JournalPath,
    pub source_identity: String,
    pub approved_target_identity: String,
    pub volume_identity: String,
    pub source_parent: JournalPath,
    pub source_parent_identity: String,
    pub destination_parent: JournalPath,
    pub destination_parent_identity: String,
    pub phase: DirectoryMoveReplaceRenamePhase,
    pub source_location: DirectoryMoveReplaceSourceLocation,
    pub target_location: DirectoryMoveReplaceTargetLocation,
    pub approved_target_children: Vec<JournalDirectoryMoveReplaceRenameChild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_cleanup: Option<JournalDirectoryMoveReplaceRenameChildIntent>,
    #[serde(default)]
    pub backup_root_disposed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalDirectoryMoveReplaceRenameChildIntent {
    pub ordinal: u32,
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryMoveReplaceRenamePhase {
    Prepared,
    NoEffect,
    DisplaceIntent,
    TargetDisplaced,
    PublishIntent,
    DestinationPublished,
    BackupCleanup,
    BackupComplete,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryMoveReplaceSourceLocation {
    Original,
    Published,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryMoveReplaceTargetLocation {
    Destination,
    Backup,
    Disposed,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalDirectoryMoveReplaceRenameChild {
    pub ordinal: u32,
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
    #[serde(default)]
    pub disposition_recorded: bool,
}


#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryRoot {
    pub source_identity: String,
    pub destination_identity: String,
    #[serde(default)]
    pub pending: Option<JournalDirectoryRootIntent>,
    #[serde(default)]
    pub disposition_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryRootIntent {
    pub source_identity: String,
    pub destination_identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryChild {
    pub ordinal: u32,
    #[serde(default)]
    pub node_kind: JournalDirectoryNodeKind,
    /// Relative UTF-16 component sequence, rooted at the moved directory.
    pub relative_components: Vec<Vec<u16>>,
    pub source_identity: String,
    pub destination_identity: String,
    pub content_fact: String,
    pub disposition_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalDirectoryChildIntent {
    pub ordinal: u32,
    #[serde(default)]
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub source_identity: String,
    pub destination_identity: String,
    pub content_fact: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalBatchRenameTarget {
    pub source: JournalPath,
    pub destination: JournalPath,
    pub source_identity: String,
    pub parent_identity: String,
    #[serde(default)]
    pub source_kind: Option<JournalBatchRenameKind>,
    #[serde(default)]
    pub reparse_tag: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalBatchRenameReceipt {
    pub source: JournalPath,
    pub destination: JournalPath,
    pub source_identity: String,
    pub parent_identity: String,
    #[serde(default)]
    pub source_kind: Option<JournalBatchRenameKind>,
    #[serde(default)]
    pub reparse_tag: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalBatchRenameKind {
    File,
    Folder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalReplaceBackup {
    pub source: JournalPath,
    pub destination: JournalPath,
    pub backup: JournalPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrossVolumeMoveReplacePhase {
    StageCreateIntent,
    StageOwned,
    StageNotCreated,
    StageReady,
    StageDiscarded,
    DisplaceIntent,
    TargetDisplaced,
    PublishIntent,
    DestinationPublished,
    SourceDeleteIntent,
    SourceDeleted,
    BackupDeleteIntent,
    BackupDeleted,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCrossVolumeMoveReplace {
    pub item: u32,
    pub source: JournalPath,
    pub stage: JournalPath,
    pub destination: JournalPath,
    pub backup: JournalPath,
    pub source_identity: String,
    #[serde(default)]
    pub stage_identity: String,
    pub approved_target_identity: String,
    pub destination_parent_identity: String,
    #[serde(default)]
    pub content_sha256: String,
    pub phase: CrossVolumeMoveReplacePhase,
}

/// Copy Replace owns a displaced folder backup until every old descendant is
/// durably accounted for. This ledger is deliberately distinct from Move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCopyReplaceCleanup {
    pub item: u32,
    pub destination: JournalPath,
    pub backup: JournalPath,
    pub root_identity: String,
    pub children: Vec<JournalCopyReplaceChild>,
    pub pending: Option<JournalCopyReplaceChildIntent>,
    #[serde(default)]
    pub root_disposition_recorded: bool,
    #[serde(default)]
    pub cleanup_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCopyReplaceChild {
    pub ordinal: u32,
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
    pub disposition_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCopyReplaceChildIntent {
    pub ordinal: u32,
    pub node_kind: JournalDirectoryNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRenameStage {
    pub source: JournalPath,
    pub stage: JournalPath,
    pub destination: JournalPath,
    pub source_identity: String,
    pub parent_identity: String,
    pub phase: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalZipCreatePhase {
    StagePlanned,
    StageOwned,
    PublishIntent,
    Published,
    StageDiscarded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalZipCreate {
    pub sources: Vec<JournalZipCreateSource>,
    pub output: JournalPath,
    pub stage: JournalPath,
    pub parent_identity: String,
    #[serde(default)]
    pub stage_identity: Option<String>,
    #[serde(default)]
    pub output_identity: Option<String>,
    pub phase: JournalZipCreatePhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalZipCreateSource {
    pub source: JournalPath,
    pub source_identity: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalZipExtractPhase {
    StagePlanned,
    StageOwned,
    PublishIntent,
    Published,
    StageDiscarded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalZipExtract {
    pub source: JournalPath,
    pub output: JournalPath,
    pub stage: JournalPath,
    pub source_identity: String,
    pub parent_identity: String,
    #[serde(default)]
    pub stage_identity: Option<String>,
    pub phase: JournalZipExtractPhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalZipExtractItem {
    pub item: u32,
    pub ledger: JournalZipExtract,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalZipExtractItemOutcome {
    Completed,
    Failed,
    CancelledBeforeStart,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalZipExtractReceipt {
    pub item: u32,
    pub outcome: JournalZipExtractItemOutcome,
    pub output: Option<JournalPath>,
}

pub(crate) fn zip_extract_transition_is_valid(
    previous: &JournalZipExtract,
    next: &JournalZipExtract,
) -> bool {
    previous.source == next.source
        && previous.output == next.output
        && previous.stage == next.stage
        && previous.source_identity == next.source_identity
        && previous.parent_identity == next.parent_identity
        && (previous.stage_identity.is_none() || previous.stage_identity == next.stage_identity)
        && matches!(
            (previous.phase, next.phase),
            (
                JournalZipExtractPhase::StagePlanned,
                JournalZipExtractPhase::StageOwned
            ) | (
                JournalZipExtractPhase::StageOwned,
                JournalZipExtractPhase::PublishIntent
            ) | (
                JournalZipExtractPhase::PublishIntent,
                JournalZipExtractPhase::Published
            ) | (
                JournalZipExtractPhase::PublishIntent,
                JournalZipExtractPhase::StageDiscarded
            ) | (
                JournalZipExtractPhase::StageOwned,
                JournalZipExtractPhase::StageDiscarded
            ) | (
                JournalZipExtractPhase::StagePlanned,
                JournalZipExtractPhase::StageDiscarded
            )
        )
        && (next.phase != JournalZipExtractPhase::StageOwned || next.stage_identity.is_some())
}

pub(crate) fn zip_create_transition_is_valid(
    previous: &JournalZipCreate,
    next: &JournalZipCreate,
) -> bool {
    previous.sources == next.sources
        && previous.output == next.output
        && previous.stage == next.stage
        && previous.parent_identity == next.parent_identity
        && (previous.stage_identity.is_none() || previous.stage_identity == next.stage_identity)
        && (previous.output_identity.is_none() || previous.output_identity == next.output_identity)
        && (next.phase != JournalZipCreatePhase::Published
            || next
                .output_identity
                .as_deref()
                .is_some_and(|id| !id.is_empty()))
        && (next.phase == JournalZipCreatePhase::Published || next.output_identity.is_none())
        && matches!(
            (previous.phase, next.phase),
            (
                JournalZipCreatePhase::StagePlanned,
                JournalZipCreatePhase::StageOwned
            ) | (
                JournalZipCreatePhase::StageOwned,
                JournalZipCreatePhase::PublishIntent
            ) | (
                JournalZipCreatePhase::PublishIntent,
                JournalZipCreatePhase::Published
            ) | (
                JournalZipCreatePhase::PublishIntent,
                JournalZipCreatePhase::StageDiscarded
            ) | (
                JournalZipCreatePhase::StageOwned,
                JournalZipCreatePhase::StageDiscarded
            ) | (
                JournalZipCreatePhase::StagePlanned,
                JournalZipCreatePhase::StageDiscarded
            )
        )
        && (next.phase != JournalZipCreatePhase::StageOwned || next.stage_identity.is_some())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct JournalDocument {
    schema: u32,
    attempts: Vec<JournalAttempt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryItem {
    RecoveryRequired(JournalAttempt),
    Unknown { reason: String },
}

/// Owns the on-disk document.  It is intentionally not an operation owner;
/// runtime sends it detached facts after accepting them.
pub struct JournalStore {
    file: SessionFile,
    document: JournalDocument,
}

enum WriterCommand {
    ReadAttempt(String, async_channel::Sender<Result<Option<JournalAttempt>, String>>),
    Upsert(
        JournalAttempt,
        Option<async_channel::Sender<Result<(), String>>>,
    ),
    RegisterDirectoryMove(
        String,
        JournalDirectoryMove,
        async_channel::Sender<Result<(), String>>,
    ),
    RegisterCrossVolumeMoveReplace(
        String,
        JournalCrossVolumeMoveReplace,
        async_channel::Sender<Result<(), String>>,
    ),
    AdvanceCrossVolumeMoveReplace(
        String,
        JournalCrossVolumeMoveReplace,
        async_channel::Sender<Result<(), String>>,
    ),
    RegisterDirectoryMoveReplaceRename(
        String,
        JournalDirectoryMoveReplaceRename,
        async_channel::Sender<Result<(), String>>,
    ),
    AdvanceDirectoryMoveReplaceRename(
        String,
        JournalDirectoryMoveReplaceRename,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveReplaceRenameCleanupIntent(
        String,
        u32,
        JournalDirectoryMoveReplaceRenameChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveReplaceRenameCleanupReceipt(
        String,
        u32,
        JournalDirectoryMoveReplaceRenameChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    CompleteDirectoryMoveReplaceRenameBackup(String, u32, async_channel::Sender<Result<(), String>>),
    RegisterCopyReplaceCleanup(
        String,
        JournalCopyReplaceCleanup,
        async_channel::Sender<Result<(), String>>,
    ),
    CopyReplaceCleanupIntent(
        String,
        u32,
        JournalCopyReplaceChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    CopyReplaceCleanupReceipt(
        String,
        u32,
        JournalCopyReplaceChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    CompleteCopyReplaceCleanup(String, u32, async_channel::Sender<Result<(), String>>),
    DirectoryMoveIntent(
        String,
        u32,
        JournalDirectoryChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveReceipt(
        String,
        u32,
        JournalDirectoryChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveRootIntent(
        String,
        u32,
        JournalDirectoryRootIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveRootReceipt(
        String,
        u32,
        JournalDirectoryRootIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    CompleteDirectoryMove(String, u32, async_channel::Sender<Result<(), String>>),
    AdvanceDirectoryMoveReplace(
        String,
        u32,
        DirectoryMoveReplacePhase,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveReplaceBackupIntent(
        String,
        u32,
        JournalDirectoryMoveReplaceChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    DirectoryMoveReplaceBackupReceipt(
        String,
        u32,
        JournalDirectoryMoveReplaceChildIntent,
        async_channel::Sender<Result<(), String>>,
    ),
    CompleteDirectoryMoveReplaceBackup(String, u32, async_channel::Sender<Result<(), String>>),
    Discard(String, async_channel::Sender<Result<bool, String>>),
}

/// Serial owner for every post-startup mutation of the journal document.
#[derive(Clone)]
pub struct JournalWriter {
    sender: async_channel::Sender<WriterCommand>,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) enum JournalWriterObservationForTest {
    Upsert {
        attempt: JournalAttempt,
        result: Result<(), String>,
    },
    ReadAttempt {
        key: String,
        result: Result<Option<JournalAttempt>, String>,
    },
}

#[cfg(test)]
struct TerminalPreUpsertGateForTest {
    key: String,
    kind: JournalKind,
    reached_path: std::path::PathBuf,
    release_path: std::path::PathBuf,
}

#[cfg(test)]
struct DirectoryMoveIntentGateForTest {
    key: String,
    item: u32,
    intent: JournalDirectoryChildIntent,
    reached_path: std::path::PathBuf,
    release_path: std::path::PathBuf,
}

#[cfg(test)]
struct DirectoryMoveRootIntentGateForTest {
    key: String,
    item: u32,
    intent: JournalDirectoryRootIntent,
    reached_path: std::path::PathBuf,
    release_path: std::path::PathBuf,
}

#[cfg(test)]
struct DirectoryMoveRootReceiptGateForTest {
    key: String,
    item: u32,
    intent: JournalDirectoryRootIntent,
    reached_path: std::path::PathBuf,
    release_path: std::path::PathBuf,
}

#[cfg(test)]
struct CopyReplaceRootReceiptGateForTest {
    key: String,
    item: u32,
    intent: JournalCopyReplaceChildIntent,
    reached_path: std::path::PathBuf,
    release_path: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    source_path: std::path::PathBuf,
    destination_path: std::path::PathBuf,
}

#[cfg(test)]
struct CopyReplaceCleanupCompleteGateForTest {
    key: String,
    item: u32,
    expected_ledger: JournalCopyReplaceCleanup,
    reached_path: std::path::PathBuf,
    release_path: std::path::PathBuf,
    runtime_token: String,
}

#[cfg(test)]
static DIRECTORY_MOVE_INTENT_GATE_FOR_TEST: std::sync::Mutex<Option<DirectoryMoveIntentGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static DIRECTORY_MOVE_ROOT_INTENT_GATE_FOR_TEST: std::sync::Mutex<Option<DirectoryMoveRootIntentGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static DIRECTORY_MOVE_ROOT_RECEIPT_GATE_FOR_TEST: std::sync::Mutex<Option<DirectoryMoveRootReceiptGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static COPY_REPLACE_ROOT_RECEIPT_GATE_FOR_TEST: std::sync::Mutex<Option<CopyReplaceRootReceiptGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static COPY_REPLACE_ROOT_RECEIPT_BEFORE_STORE_GATE_FOR_TEST: std::sync::Mutex<Option<CopyReplaceRootReceiptGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static COPY_REPLACE_CLEANUP_COMPLETE_GATE_FOR_TEST: std::sync::Mutex<Option<CopyReplaceCleanupCompleteGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static COPY_REPLACE_CLEANUP_COMPLETE_AFTER_STORE_GATE_FOR_TEST: std::sync::Mutex<Option<CopyReplaceCleanupCompleteGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn gate_copy_replace_cleanup_complete_before_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
) {
    let gate = {
        let mut active = COPY_REPLACE_CLEANUP_COMPLETE_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace cleanup-complete test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| gate.key == key && gate.item == item) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    assert_eq!(gate.item, item);
    assert_eq!(gate.expected_ledger.item, item);
    assert!(gate.expected_ledger.root_disposition_recorded);
    assert!(gate.expected_ledger.pending.is_none());
    assert!(!gate.expected_ledger.cleanup_complete);
    let attempt = store.attempt(key).expect("Running predecessor remains present").clone();
    assert_eq!(attempt.state, JournalState::Running);
    assert!(attempt.terminal_outcome.is_none());
    let ledger = attempt.copy_replace_cleanups.iter().find(|ledger| ledger.item == item)
        .expect("exact Copy Replace cleanup ledger remains present");
    assert_eq!(ledger, &gate.expected_ledger, "writer predecessor matches the full exact expected cleanup ledger");
    let journal_path = store.file.paths().primary.clone();
    let source_path = attempt.sources.first().and_then(JournalPath::to_path)
        .expect("Copy Replace predecessor retains its exact source path");
    let destination_path = attempt.destination.as_ref().and_then(JournalPath::to_path)
        .expect("Copy Replace predecessor retains its exact destination path");
    let primary = std::fs::read(&journal_path).expect("read durable Copy Replace cleanup-complete predecessor");
    let checkpoint = serde_json::json!({
        "phase": "copy-replace-cleanup-complete-before-store",
        "key": key,
        "item": item,
        "attempt": attempt,
        "ledger": ledger,
        "primary_bytes": primary,
        "journal_path": journal_path.to_string_lossy(),
        "source_path": source_path.to_string_lossy(),
        "destination_path": destination_path.to_string_lossy(),
        "runtime_token": gate.runtime_token,
        "runtime_complete_pending": true,
        "runtime_no_terminal": true,
        "runtime_no_undo": true,
        "worker_lane_claimed": true,
    });
    use std::io::Write;
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    let mut payload = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&payload_path).expect("create Copy Replace cleanup-complete checkpoint payload");
    payload.write_all(&serde_json::to_vec(&checkpoint).unwrap())
        .expect("write Copy Replace cleanup-complete checkpoint payload");
    payload.sync_all().expect("flush Copy Replace cleanup-complete checkpoint payload");
    drop(payload);
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&gate.reached_path).expect("publish Copy Replace cleanup-complete gate marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !gate.release_path.exists() {
        assert!(std::time::Instant::now() < deadline,
            "test did not release Copy Replace cleanup-complete JournalWriter gate");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
fn gate_copy_replace_cleanup_complete_after_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
    result: &Result<(), String>,
) {
    if result.is_err() {
        return;
    }
    let gate = {
        let mut active = COPY_REPLACE_CLEANUP_COMPLETE_AFTER_STORE_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace cleanup-complete after-store test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| gate.key == key && gate.item == item) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    assert!(result.is_ok(), "exact cleanup-complete store must succeed before the after-store test gate");
    assert_eq!(gate.expected_ledger.item, item);
    assert!(gate.expected_ledger.root_disposition_recorded);
    assert!(gate.expected_ledger.pending.is_none());
    assert!(!gate.expected_ledger.cleanup_complete);

    let attempt = store.attempt(key).expect("Running attempt remains present after cleanup-complete store").clone();
    assert_eq!(attempt.state, JournalState::Running);
    assert!(attempt.terminal_outcome.is_none());
    let mut expected_ledger = gate.expected_ledger.clone();
    expected_ledger.cleanup_complete = true;
    let ledger = attempt.copy_replace_cleanups.iter().find(|ledger| ledger.item == item)
        .expect("exact Copy Replace cleanup ledger remains present after store");
    assert_eq!(ledger, &expected_ledger, "writer persisted the exact predecessor ledger with cleanup_complete set");

    let journal_path = store.file.paths().primary.clone();
    let primary = std::fs::read(&journal_path).expect("read durable Copy Replace cleanup-complete journal bytes");
    let document: JournalDocument = serde_json::from_slice(&primary)
        .expect("parse the actual persisted Copy Replace cleanup-complete primary");
    let persisted = document.attempts.iter().find(|candidate| candidate.key == key)
        .expect("persisted primary contains the exact attempt");
    assert_eq!(persisted, &attempt, "persisted primary bytes encode the writer's exact attempt");
    let persisted_ledger = persisted.copy_replace_cleanups.iter().find(|ledger| ledger.item == item)
        .expect("persisted primary contains the exact cleanup ledger");
    assert_eq!(persisted_ledger, &expected_ledger);

    let source_path = attempt.sources.first().and_then(JournalPath::to_path)
        .expect("Copy Replace attempt retains its exact source path");
    let destination_path = attempt.destination.as_ref().and_then(JournalPath::to_path)
        .expect("Copy Replace attempt retains its exact destination path");
    let checkpoint = serde_json::json!({
        "phase": "copy-replace-cleanup-complete-after-store-before-ack",
        "key": key,
        "item": item,
        "attempt": attempt,
        "ledger": persisted_ledger,
        "primary_bytes": primary,
        "journal_path": journal_path.to_string_lossy(),
        "source_path": source_path.to_string_lossy(),
        "destination_path": destination_path.to_string_lossy(),
        "runtime_token": gate.runtime_token,
        "runtime_complete_pending": true,
        "runtime_no_terminal": true,
        "runtime_no_undo": true,
        "worker_lane_claimed": true,
    });
    use std::io::Write;
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    let mut payload = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&payload_path).expect("create Copy Replace after-store checkpoint payload");
    payload.write_all(&serde_json::to_vec(&checkpoint).unwrap())
        .expect("write Copy Replace after-store checkpoint payload");
    payload.sync_all().expect("flush Copy Replace after-store checkpoint payload");
    drop(payload);
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&gate.reached_path).expect("publish Copy Replace after-store gate marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !gate.release_path.exists() {
        assert!(std::time::Instant::now() < deadline, "test did not release Copy Replace after-store gate");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
fn gate_copy_replace_root_receipt_before_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
    intent: &JournalCopyReplaceChildIntent,
) {
    let gate = {
        let mut active = COPY_REPLACE_ROOT_RECEIPT_BEFORE_STORE_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace pre-store root receipt test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| gate.key == key && gate.item == item && gate.intent == *intent) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    assert_eq!(intent.node_kind, JournalDirectoryNodeKind::Directory);
    assert!(intent.relative_components.is_empty());
    assert_eq!(intent.content_fact, "root");
    let attempt = store.attempt(key).expect("Running predecessor remains present").clone();
    assert_eq!(attempt.state, JournalState::Running);
    assert!(attempt.terminal_outcome.is_none());
    let ledger = attempt.copy_replace_cleanups.iter().find(|ledger| ledger.item == item)
        .expect("exact Copy Replace cleanup ledger remains present");
    assert!(!ledger.cleanup_complete);
    assert!(!ledger.root_disposition_recorded);
    assert!(ledger.children.iter().all(|child| child.disposition_recorded));
    assert_eq!(ledger.pending.as_ref(), Some(intent));
    let primary = std::fs::read(&gate.journal_path).expect("read durable Copy Replace root-receipt predecessor");
    let checkpoint = serde_json::json!({
        "phase": "copy-replace-root-disposition-before-receipt-store",
        "key": key,
        "item": item,
        "intent": intent,
        "attempt": attempt,
        "primary_bytes": primary,
        "journal_path": gate.journal_path.to_string_lossy(),
        "source_path": gate.source_path.to_string_lossy(),
        "destination_path": gate.destination_path.to_string_lossy(),
    });
    use std::io::Write;
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    let mut payload = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&payload_path).expect("create Copy Replace pre-store root-receipt checkpoint payload");
    payload.write_all(&serde_json::to_vec(&checkpoint).unwrap())
        .expect("write Copy Replace pre-store root-receipt checkpoint payload");
    payload.sync_all().expect("flush Copy Replace pre-store root-receipt checkpoint payload");
    drop(payload);
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&gate.reached_path).expect("publish Copy Replace pre-store root-receipt gate marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !gate.release_path.exists() {
        assert!(std::time::Instant::now() < deadline, "test did not release Copy Replace pre-store root-receipt gate");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
fn gate_copy_replace_root_receipt_after_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
    intent: &JournalCopyReplaceChildIntent,
    result: &Result<(), String>,
) {
    let gate = {
        let mut active = COPY_REPLACE_ROOT_RECEIPT_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace root receipt test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| {
            gate.key == key && gate.item == item && gate.intent == *intent
        }) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    assert!(result.is_ok(), "exact root receipt store must succeed before the test gate");

    assert_eq!(intent.node_kind, JournalDirectoryNodeKind::Directory);
    assert!(intent.relative_components.is_empty());
    assert_eq!(intent.content_fact, "root");
    let attempt = store.attempt(key).expect("Running predecessor remains present").clone();
    assert_eq!(attempt.state, JournalState::Running);
    assert!(attempt.terminal_outcome.is_none());
    let ledger = attempt.copy_replace_cleanups.iter().find(|ledger| ledger.item == item)
        .expect("exact Copy Replace cleanup ledger remains present");
    assert!(!ledger.cleanup_complete);
    assert!(ledger.root_disposition_recorded);
    assert!(ledger.children.iter().all(|child| child.disposition_recorded));
    assert!(ledger.pending.is_none());
    let primary = std::fs::read(&gate.journal_path).expect("read durable Copy Replace root-receipt journal");
    let checkpoint = serde_json::json!({
        "phase": "copy-replace-root-receipt-after-store-before-ack",
        "key": key,
        "item": item,
        "intent": intent,
        "attempt": attempt,
        "primary_bytes": primary,
        "journal_path": gate.journal_path.to_string_lossy(),
        "source_path": gate.source_path.to_string_lossy(),
        "destination_path": gate.destination_path.to_string_lossy(),
    });
    use std::io::Write;
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    let mut payload = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&payload_path).expect("create Copy Replace root-receipt checkpoint payload");
    payload.write_all(&serde_json::to_vec(&checkpoint).unwrap())
        .expect("write Copy Replace root-receipt checkpoint payload");
    payload.sync_all().expect("flush Copy Replace root-receipt checkpoint payload");
    drop(payload);
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&gate.reached_path).expect("publish Copy Replace root-receipt gate marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !gate.release_path.exists() {
        assert!(std::time::Instant::now() < deadline,
            "test did not release Copy Replace root-receipt JournalWriter gate");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
fn gate_directory_move_root_receipt_before_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
    intent: &JournalDirectoryRootIntent,
) {
    let gate = {
        let mut active = DIRECTORY_MOVE_ROOT_RECEIPT_GATE_FOR_TEST
            .lock()
            .expect("directory move root receipt test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| {
            gate.key == key && gate.item == item && gate.intent == *intent
        }) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    let attempt = store.attempt(key).expect("Running predecessor remains present").clone();
    assert_eq!(attempt.state, JournalState::Running);
    assert!(attempt.terminal_outcome.is_none());
    let ledger = attempt.directory_moves.iter().find(|ledger| ledger.item == item)
        .expect("exact directory Move ledger remains present");
    assert!(!ledger.cleanup_complete);
    assert!(ledger.children.iter().all(|child| child.disposition_recorded));
    let root = ledger.root.as_ref().expect("source root ledger remains present");
    assert_eq!(root.pending.as_ref(), Some(intent));
    assert!(!root.disposition_recorded);
    let primary = std::fs::read(std::env::var_os("GPUI_FILER_SHELL_DIRECTORY_MOVE_ROOT_RECEIPT_JOURNAL")
        .expect("test gate journal path")).expect("read durable root-receipt predecessor");
    let checkpoint = serde_json::json!({
        "phase": "directory-move-root-disposition-before-receipt-store",
        "key": key,
        "item": item,
        "intent": intent,
        "attempt": attempt,
        "primary_bytes": primary,
    });
    use std::io::Write;
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    let mut payload = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&payload_path).expect("create durable root-receipt checkpoint payload");
    payload.write_all(&serde_json::to_vec(&checkpoint).unwrap())
        .expect("write durable root-receipt checkpoint payload");
    payload.sync_all().expect("flush durable root-receipt checkpoint payload");
    drop(payload);
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&gate.reached_path).expect("publish durable root-receipt gate marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !gate.release_path.exists() {
        assert!(std::time::Instant::now() < deadline,
            "test did not release root-receipt JournalWriter gate");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
fn gate_directory_move_root_intent_after_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
    intent: &JournalDirectoryRootIntent,
    result: &Result<(), String>,
) {
    if result.is_err() {
        return;
    }
    let gate = {
        let mut active = DIRECTORY_MOVE_ROOT_INTENT_GATE_FOR_TEST
            .lock()
            .expect("directory move root intent test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| {
            gate.key == key && gate.item == item && gate.intent == *intent
        }) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    let attempt = store.attempt(key).expect("Running predecessor remains present").clone();
    assert_eq!(attempt.state, JournalState::Running);
    let ledger = attempt.directory_moves.iter().find(|ledger| ledger.item == item)
        .expect("exact directory Move ledger remains present");
    assert_eq!(ledger.root.as_ref().and_then(|root| root.pending.as_ref()), Some(intent));
    assert!(ledger.children.iter().all(|child| child.disposition_recorded),
        "every original child receipt is durable before source-root intent");
    let primary = std::fs::read(std::env::var_os("GPUI_FILER_SHELL_DIRECTORY_MOVE_ROOT_INTENT_JOURNAL")
        .expect("test gate journal path")).expect("read durable root-intent predecessor");
    let checkpoint = serde_json::json!({
        "phase": "directory-move-root-intent-persisted-before-shell-ack",
        "key": key,
        "item": item,
        "intent": intent,
        "attempt": attempt,
        "primary_bytes": primary,
    });
    use std::io::Write;
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    let mut payload = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&payload_path).expect("create durable root-intent checkpoint payload");
    payload.write_all(&serde_json::to_vec(&checkpoint).unwrap())
        .expect("write durable root-intent checkpoint payload");
    payload.sync_all().expect("flush durable root-intent checkpoint payload");
    drop(payload);
    std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&gate.reached_path).expect("publish durable root-intent ACK gate marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !gate.release_path.exists() {
        assert!(std::time::Instant::now() < deadline,
            "test did not release root-intent JournalWriter ACK gate");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
fn gate_directory_move_intent_before_store_for_test(
    store: &JournalStore,
    key: &str,
    item: u32,
    intent: &JournalDirectoryChildIntent,
) {
    let gate = {
        let mut active = DIRECTORY_MOVE_INTENT_GATE_FOR_TEST
            .lock()
            .expect("directory move intent test gate mutex poisoned");
        if active.as_ref().is_some_and(|gate| {
            gate.key == key && gate.item == item && gate.intent == *intent
        }) {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    // JournalWriter専用スレッドだけを止め、Shell pumpのACK待ちは継続可能にする。
    let predecessor = store
        .attempt(key)
        .expect("Running predecessor exists before child cleanup intent")
        .clone();
    assert_eq!(predecessor.state, JournalState::Running);
    let checkpoint = serde_json::json!({
        "phase": "directory-move-child-intent-command-accepted-before-store-transition",
        "key": key,
        "item": item,
        "intent": intent,
        "attempt": predecessor,
        "primary_bytes": std::fs::read(std::env::var_os("GPUI_FILER_SHELL_DIRECTORY_MOVE_INTENT_JOURNAL")
            .expect("test gate journal path")).expect("read exact durable predecessor before mutation"),
    });
    let checkpoint_bytes = serde_json::to_vec(&checkpoint).unwrap();
    let payload_path = gate.reached_path.with_extension("checkpoint.json");
    assert!(!gate.reached_path.exists(), "checkpoint ready marker must be fresh");
    assert!(!payload_path.exists(), "checkpoint payload must be fresh");
    use std::io::Write;
    let mut payload = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&payload_path)
        .expect("create exact directory move checkpoint payload");
    payload.write_all(&checkpoint_bytes).expect("write exact directory move checkpoint payload");
    payload.sync_all().expect("flush exact directory move checkpoint payload");
    drop(payload);
    // Ready marker creation is atomic; the parent reads the already-flushed payload after it appears.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&gate.reached_path)
        .expect("publish exact directory move checkpoint ready marker");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !gate.release_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "test did not release directory move intent JournalWriter gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
static TERMINAL_PRE_UPSERT_GATE_FOR_TEST: std::sync::Mutex<Option<TerminalPreUpsertGateForTest>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn gate_terminal_before_upsert_for_test(attempt: &JournalAttempt) {
    if attempt.state != JournalState::Terminal {
        return;
    }
    let gate = {
        let mut active = TERMINAL_PRE_UPSERT_GATE_FOR_TEST
            .lock()
            .expect("terminal writer test gate mutex poisoned");
        if active
            .as_ref()
            .is_some_and(|gate| gate.key == attempt.key && gate.kind == attempt.kind)
        {
            active.take()
        } else {
            None
        }
    };
    let Some(gate) = gate else { return };

    // この待機はJournalWriter専用スレッド上で行い、GPUIのpumpやUIを止めない。
    let checkpoint = serde_json::json!({
        "phase": "terminal-command-accepted-before-store-upsert",
        "key": attempt.key,
        "attempt": attempt,
    });
    write_test_artifact_atomically(
        &gate.reached_path,
        &serde_json::to_vec(&checkpoint).unwrap(),
    )
    .expect("publish exact pre-upsert writer checkpoint");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !gate.release_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "test did not release terminal JournalWriter gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
fn write_test_artifact_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);

    assert!(!path.exists(), "test artifact target must be fresh");
    let name = path.file_name().expect("test artifact has a filename").to_string_lossy();
    let temp_path = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp_path);
    }
    result
}

#[cfg(test)]
pub(crate) struct JournalWriterTestDriver {
    receiver: async_channel::Receiver<WriterCommand>,
}

#[cfg(test)]
pub(crate) struct JournalWriterUpsertForTest {
    pub attempt: JournalAttempt,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) struct JournalWriterDiscardForTest {
    pub key: String,
    reply: async_channel::Sender<Result<bool, String>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JournalWriterCrossVolumeMoveReplaceActionForTest {
    Register,
    Advance,
}

#[cfg(test)]
pub(crate) struct JournalWriterCrossVolumeMoveReplaceForTest {
    pub key: String,
    pub ledger: JournalCrossVolumeMoveReplace,
    pub action: JournalWriterCrossVolumeMoveReplaceActionForTest,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) struct JournalWriterDirectoryMoveManifestForTest {
    pub key: String,
    pub ledger: JournalDirectoryMove,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) struct JournalWriterDirectoryMoveTransitionForTest<T> {
    pub key: String,
    pub item: u32,
    pub value: T,
    pub receipt: bool,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) struct JournalWriterDirectoryMoveCompleteForTest {
    pub key: String,
    pub item: u32,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) struct JournalWriterDirectoryMoveReplacePhaseForTest {
    pub key: String,
    pub item: u32,
    pub phase: DirectoryMoveReplacePhase,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) struct JournalWriterDirectoryMoveReplaceBackupCompleteForTest {
    pub key: String,
    pub item: u32,
    reply: async_channel::Sender<Result<(), String>>,
}

#[cfg(test)]
pub(crate) enum JournalWriterCommandForTest {
    Upsert(JournalWriterUpsertForTest),
    UnacknowledgedUpsert(JournalAttempt),
    CrossVolumeMoveReplace(JournalWriterCrossVolumeMoveReplaceForTest),
    DirectoryMoveManifest(JournalWriterDirectoryMoveManifestForTest),
    DirectoryMoveChild(JournalWriterDirectoryMoveTransitionForTest<JournalDirectoryChildIntent>),
    DirectoryMoveRoot(JournalWriterDirectoryMoveTransitionForTest<JournalDirectoryRootIntent>),
    DirectoryMoveComplete(JournalWriterDirectoryMoveCompleteForTest),
    DirectoryMoveReplacePhase(JournalWriterDirectoryMoveReplacePhaseForTest),
    DirectoryMoveReplaceBackupChild(
        JournalWriterDirectoryMoveTransitionForTest<JournalDirectoryMoveReplaceChildIntent>,
    ),
    DirectoryMoveReplaceBackupComplete(JournalWriterDirectoryMoveReplaceBackupCompleteForTest),
}

#[cfg(test)]
impl JournalWriterTestDriver {
    pub(crate) fn try_command(&self) -> Option<JournalWriterCommandForTest> {
        match self.receiver.try_recv().ok()? {
            WriterCommand::ReadAttempt(_, _) => None,
            WriterCommand::Upsert(attempt, Some(reply)) => Some(
                JournalWriterCommandForTest::Upsert(JournalWriterUpsertForTest { attempt, reply }),
            ),
            WriterCommand::Upsert(attempt, None) => {
                Some(JournalWriterCommandForTest::UnacknowledgedUpsert(attempt))
            }
            WriterCommand::RegisterCrossVolumeMoveReplace(key, ledger, reply) => {
                Some(JournalWriterCommandForTest::CrossVolumeMoveReplace(
                    JournalWriterCrossVolumeMoveReplaceForTest {
                        key,
                        ledger,
                        action: JournalWriterCrossVolumeMoveReplaceActionForTest::Register,
                        reply,
                    },
                ))
            }
            WriterCommand::AdvanceCrossVolumeMoveReplace(key, ledger, reply) => {
                Some(JournalWriterCommandForTest::CrossVolumeMoveReplace(
                    JournalWriterCrossVolumeMoveReplaceForTest {
                        key,
                        ledger,
                        action: JournalWriterCrossVolumeMoveReplaceActionForTest::Advance,
                        reply,
                    },
                ))
            }
            WriterCommand::RegisterDirectoryMove(key, ledger, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveManifest(
                    JournalWriterDirectoryMoveManifestForTest { key, ledger, reply },
                ))
            }
            WriterCommand::DirectoryMoveIntent(key, item, value, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveChild(
                    JournalWriterDirectoryMoveTransitionForTest {
                        key,
                        item,
                        value,
                        receipt: false,
                        reply,
                    },
                ))
            }
            WriterCommand::DirectoryMoveReceipt(key, item, value, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveChild(
                    JournalWriterDirectoryMoveTransitionForTest {
                        key,
                        item,
                        value,
                        receipt: true,
                        reply,
                    },
                ))
            }
            WriterCommand::DirectoryMoveRootIntent(key, item, value, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveRoot(
                    JournalWriterDirectoryMoveTransitionForTest {
                        key,
                        item,
                        value,
                        receipt: false,
                        reply,
                    },
                ))
            }
            WriterCommand::DirectoryMoveRootReceipt(key, item, value, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveRoot(
                    JournalWriterDirectoryMoveTransitionForTest {
                        key,
                        item,
                        value,
                        receipt: true,
                        reply,
                    },
                ))
            }
            WriterCommand::CompleteDirectoryMove(key, item, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveComplete(
                    JournalWriterDirectoryMoveCompleteForTest { key, item, reply },
                ))
            }
            WriterCommand::AdvanceDirectoryMoveReplace(key, item, phase, reply) => {
                Some(JournalWriterCommandForTest::DirectoryMoveReplacePhase(
                    JournalWriterDirectoryMoveReplacePhaseForTest { key, item, phase, reply },
                ))
            }
            WriterCommand::DirectoryMoveReplaceBackupIntent(key, item, value, reply) => Some(
                JournalWriterCommandForTest::DirectoryMoveReplaceBackupChild(
                    JournalWriterDirectoryMoveTransitionForTest {
                        key,
                        item,
                        value,
                        receipt: false,
                        reply,
                    },
                ),
            ),
            WriterCommand::DirectoryMoveReplaceBackupReceipt(key, item, value, reply) => Some(
                JournalWriterCommandForTest::DirectoryMoveReplaceBackupChild(
                    JournalWriterDirectoryMoveTransitionForTest {
                        key,
                        item,
                        value,
                        receipt: true,
                        reply,
                    },
                ),
            ),
            WriterCommand::CompleteDirectoryMoveReplaceBackup(key, item, reply) => Some(
                JournalWriterCommandForTest::DirectoryMoveReplaceBackupComplete(
                    JournalWriterDirectoryMoveReplaceBackupCompleteForTest { key, item, reply },
                ),
            ),
            _ => panic!("journal test driver received an unexpected command"),
        }
    }

    pub(crate) fn try_upsert(&self) -> Option<JournalWriterUpsertForTest> {
        match self.receiver.try_recv().ok()? {
            WriterCommand::Upsert(attempt, Some(reply)) => {
                Some(JournalWriterUpsertForTest { attempt, reply })
            }
            _ => None,
        }
    }

    pub(crate) fn try_discard(&self) -> Option<JournalWriterDiscardForTest> {
        match self.receiver.try_recv().ok()? {
            WriterCommand::Discard(key, reply) => Some(JournalWriterDiscardForTest { key, reply }),
            _ => panic!("journal test driver received a non-Discard command while polling discard"),
        }
    }

    pub(crate) fn try_cross_volume_move_replace(
        &self,
    ) -> Option<JournalWriterCrossVolumeMoveReplaceForTest> {
        match self.receiver.try_recv().ok()? {
            WriterCommand::RegisterCrossVolumeMoveReplace(key, ledger, reply) => {
                Some(JournalWriterCrossVolumeMoveReplaceForTest {
                    key,
                    ledger,
                    action: JournalWriterCrossVolumeMoveReplaceActionForTest::Register,
                    reply,
                })
            }
            WriterCommand::AdvanceCrossVolumeMoveReplace(key, ledger, reply) => {
                Some(JournalWriterCrossVolumeMoveReplaceForTest {
                    key,
                    ledger,
                    action: JournalWriterCrossVolumeMoveReplaceActionForTest::Advance,
                    reply,
                })
            }
            _ => panic!("journal test driver received a non-cross-volume Replace phase command"),
        }
    }
}

#[cfg(test)]
impl JournalWriterUpsertForTest {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("journal pump is awaiting the test writer receipt");
    }
}

#[cfg(test)]
impl JournalWriterDiscardForTest {
    pub(crate) fn reply(self, result: Result<bool, String>) {
        self.reply
            .try_send(result)
            .expect("journal discard is awaiting the test writer receipt");
    }
}

#[cfg(test)]
impl JournalWriterCrossVolumeMoveReplaceForTest {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("Shell pump is awaiting the cross-volume Move Replace writer receipt");
    }
}

#[cfg(test)]
impl JournalWriterDirectoryMoveManifestForTest {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("Shell pump awaits directory-move manifest receipt");
    }
}

#[cfg(test)]
impl<T> JournalWriterDirectoryMoveTransitionForTest<T> {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("Shell pump awaits directory-move transition receipt");
    }
}

#[cfg(test)]
impl JournalWriterDirectoryMoveCompleteForTest {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("Shell pump awaits directory-move completion receipt");
    }
}

#[cfg(test)]
impl JournalWriterCommandForTest {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Upsert(_) => "Upsert",
            Self::UnacknowledgedUpsert(_) => "UnacknowledgedUpsert",
            Self::CrossVolumeMoveReplace(_) => "CrossVolumeMoveReplace",
            Self::DirectoryMoveManifest(_) => "DirectoryMoveManifest",
            Self::DirectoryMoveChild(_) => "DirectoryMoveChild",
            Self::DirectoryMoveRoot(_) => "DirectoryMoveRoot",
            Self::DirectoryMoveComplete(_) => "DirectoryMoveComplete",
            Self::DirectoryMoveReplacePhase(_) => "DirectoryMoveReplacePhase",
            Self::DirectoryMoveReplaceBackupChild(_) => "DirectoryMoveReplaceBackupChild",
            Self::DirectoryMoveReplaceBackupComplete(_) => "DirectoryMoveReplaceBackupComplete",
        }
    }

    /// Persist the captured command through the real serial writer before the
    /// Shell test relays its actual result through the captured ACK sender.
    pub(crate) fn persist_to(&self, writer: &JournalWriter) -> Result<(), String> {
        fn result(receiver: async_channel::Receiver<Result<(), String>>) -> Result<(), String> {
            receiver
                .recv_blocking()
                .unwrap_or_else(|_| Err("real test journal writer stopped".into()))
        }

        match self {
            Self::Upsert(command) => result(writer.upsert_with_reply(command.attempt.clone())),
            Self::UnacknowledgedUpsert(attempt) => writer.upsert(attempt.clone()),
            Self::CrossVolumeMoveReplace(command) => result(match command.action {
                JournalWriterCrossVolumeMoveReplaceActionForTest::Register => writer
                    .register_cross_volume_move_replace(command.key.clone(), command.ledger.clone()),
                JournalWriterCrossVolumeMoveReplaceActionForTest::Advance => writer
                    .advance_cross_volume_move_replace(command.key.clone(), command.ledger.clone()),
            }),
            Self::DirectoryMoveManifest(command) => result(
                writer.register_directory_move(command.key.clone(), command.ledger.clone()),
            ),
            Self::DirectoryMoveChild(command) => result(if command.receipt {
                writer.record_directory_move_receipt(
                    command.key.clone(),
                    command.item,
                    command.value.clone(),
                )
            } else {
                writer.record_directory_move_intent(
                    command.key.clone(),
                    command.item,
                    command.value.clone(),
                )
            }),
            Self::DirectoryMoveRoot(command) => result(if command.receipt {
                writer.record_directory_move_root_receipt(
                    command.key.clone(),
                    command.item,
                    command.value.clone(),
                )
            } else {
                writer.record_directory_move_root_intent(
                    command.key.clone(),
                    command.item,
                    command.value.clone(),
                )
            }),
            Self::DirectoryMoveComplete(command) => result(
                writer.complete_directory_move(command.key.clone(), command.item),
            ),
            Self::DirectoryMoveReplacePhase(command) => result(writer
                .advance_directory_move_replace(command.key.clone(), command.item, command.phase)),
            Self::DirectoryMoveReplaceBackupChild(command) => result(if command.receipt {
                writer.record_directory_move_replace_backup_receipt(
                    command.key.clone(),
                    command.item,
                    command.value.clone(),
                )
            } else {
                writer.record_directory_move_replace_backup_intent(
                    command.key.clone(),
                    command.item,
                    command.value.clone(),
                )
            }),
            Self::DirectoryMoveReplaceBackupComplete(command) => result(
                writer.complete_directory_move_replace_backup(command.key.clone(), command.item),
            ),
        }
    }
}

#[cfg(test)]
impl JournalWriterDirectoryMoveReplacePhaseForTest {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("Shell pump awaits directory-move Replace phase receipt");
    }
}

#[cfg(test)]
impl JournalWriterDirectoryMoveReplaceBackupCompleteForTest {
    pub(crate) fn reply(self, result: Result<(), String>) {
        self.reply
            .try_send(result)
            .expect("Shell pump awaits directory-move Replace backup completion receipt");
    }
}

impl JournalWriter {
    #[cfg(test)]
    pub(crate) fn gate_next_copy_terminal_before_upsert_for_test(
        key: String,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        Self::gate_next_kind_terminal_before_upsert_for_test(
            key,
            JournalKind::Copy,
            reached_path,
            release_path,
        );
    }

    #[cfg(test)]
    pub(crate) fn gate_next_trash_terminal_before_upsert_for_test(
        key: String,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        Self::gate_next_kind_terminal_before_upsert_for_test(
            key,
            JournalKind::Trash,
            reached_path,
            release_path,
        );
    }

    #[cfg(test)]
    pub(crate) fn gate_next_move_terminal_before_upsert_for_test(
        key: String,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        Self::gate_next_kind_terminal_before_upsert_for_test(
            key,
            JournalKind::Move,
            reached_path,
            release_path,
        );
    }

    #[cfg(test)]
    pub(crate) fn gate_directory_move_intent_before_store_for_test(
        key: String,
        item: u32,
        intent: JournalDirectoryChildIntent,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        let mut active = DIRECTORY_MOVE_INTENT_GATE_FOR_TEST
            .lock()
            .expect("directory move intent test gate mutex poisoned");
        assert!(active.is_none(), "directory move intent test gate already armed");
        *active = Some(DirectoryMoveIntentGateForTest {
            key,
            item,
            intent,
            reached_path,
            release_path,
        });
    }

    #[cfg(test)]
    pub(crate) fn gate_directory_move_root_intent_after_store_for_test(
        key: String,
        item: u32,
        intent: JournalDirectoryRootIntent,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        let mut active = DIRECTORY_MOVE_ROOT_INTENT_GATE_FOR_TEST
            .lock()
            .expect("directory move root intent test gate mutex poisoned");
        assert!(active.is_none(), "directory move root intent test gate already armed");
        *active = Some(DirectoryMoveRootIntentGateForTest {
            key,
            item,
            intent,
            reached_path,
            release_path,
        });
    }

    #[cfg(test)]
    pub(crate) fn gate_directory_move_root_receipt_before_store_for_test(
        key: String,
        item: u32,
        intent: JournalDirectoryRootIntent,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        let mut active = DIRECTORY_MOVE_ROOT_RECEIPT_GATE_FOR_TEST
            .lock()
            .expect("directory move root receipt test gate mutex poisoned");
        assert!(active.is_none(), "directory move root receipt test gate already armed");
        *active = Some(DirectoryMoveRootReceiptGateForTest {
            key,
            item,
            intent,
            reached_path,
            release_path,
        });
    }

    #[cfg(test)]
    pub(crate) fn gate_copy_replace_root_receipt_after_store_for_test(
        key: String,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
        journal_path: std::path::PathBuf,
        source_path: std::path::PathBuf,
        destination_path: std::path::PathBuf,
    ) {
        let mut active = COPY_REPLACE_ROOT_RECEIPT_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace root receipt test gate mutex poisoned");
        assert!(active.is_none(), "Copy Replace root receipt test gate already armed");
        *active = Some(CopyReplaceRootReceiptGateForTest {
            key,
            item,
            intent,
            reached_path,
            release_path,
            journal_path,
            source_path,
            destination_path,
        });
    }

    #[cfg(test)]
    pub(crate) fn gate_copy_replace_root_receipt_before_store_for_test(
        key: String,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
        journal_path: std::path::PathBuf,
        source_path: std::path::PathBuf,
        destination_path: std::path::PathBuf,
    ) {
        let mut active = COPY_REPLACE_ROOT_RECEIPT_BEFORE_STORE_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace pre-store root receipt test gate mutex poisoned");
        assert!(active.is_none(), "Copy Replace pre-store root receipt test gate already armed");
        *active = Some(CopyReplaceRootReceiptGateForTest {
            key,
            item,
            intent,
            reached_path,
            release_path,
            journal_path,
            source_path,
            destination_path,
        });
    }

    #[cfg(test)]
    pub(crate) fn gate_copy_replace_cleanup_complete_before_store_for_test(
        key: String,
        item: u32,
        expected_ledger: JournalCopyReplaceCleanup,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
        runtime_token: String,
    ) {
        let mut active = COPY_REPLACE_CLEANUP_COMPLETE_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace cleanup-complete test gate mutex poisoned");
        assert!(active.is_none(), "Copy Replace cleanup-complete test gate already armed");
        *active = Some(CopyReplaceCleanupCompleteGateForTest {
            key,
            item,
            expected_ledger,
            reached_path,
            release_path,
            runtime_token,
        });
    }

    #[cfg(test)]
    pub(crate) fn gate_copy_replace_cleanup_complete_after_store_for_test(
        key: String,
        item: u32,
        expected_ledger: JournalCopyReplaceCleanup,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
        runtime_token: String,
    ) {
        let mut active = COPY_REPLACE_CLEANUP_COMPLETE_AFTER_STORE_GATE_FOR_TEST
            .lock()
            .expect("Copy Replace cleanup-complete after-store test gate mutex poisoned");
        assert!(active.is_none(), "Copy Replace cleanup-complete after-store test gate already armed");
        *active = Some(CopyReplaceCleanupCompleteGateForTest {
            key,
            item,
            expected_ledger,
            reached_path,
            release_path,
            runtime_token,
        });
    }

    #[cfg(test)]
    fn gate_next_kind_terminal_before_upsert_for_test(
        key: String,
        kind: JournalKind,
        reached_path: std::path::PathBuf,
        release_path: std::path::PathBuf,
    ) {
        let mut active = TERMINAL_PRE_UPSERT_GATE_FOR_TEST
            .lock()
            .expect("terminal writer test gate mutex poisoned");
        assert!(active.is_none(), "terminal writer test gate already armed");
        *active = Some(TerminalPreUpsertGateForTest {
            key,
            kind,
            reached_path,
            release_path,
        });
    }

    #[cfg(test)]
    pub(crate) fn test_driver() -> (Self, JournalWriterTestDriver) {
        let (sender, receiver) = async_channel::unbounded();
        (Self { sender }, JournalWriterTestDriver { receiver })
    }

    fn acknowledged(
        &self,
        command: impl FnOnce(async_channel::Sender<Result<(), String>>) -> WriterCommand,
    ) -> async_channel::Receiver<Result<(), String>> {
        let (reply, receiver) = async_channel::bounded(1);
        if self.sender.send_blocking(command(reply.clone())).is_err() {
            let _ = reply.send_blocking(Err("operation journal writer is unavailable".into()));
        }
        receiver
    }

    pub fn register_directory_move(
        &self,
        key: String,
        ledger: JournalDirectoryMove,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::RegisterDirectoryMove(key, ledger, reply))
    }

    pub fn register_cross_volume_move_replace(
        &self,
        key: String,
        ledger: JournalCrossVolumeMoveReplace,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::RegisterCrossVolumeMoveReplace(key, ledger, reply))
    }

    pub fn advance_cross_volume_move_replace(
        &self,
        key: String,
        ledger: JournalCrossVolumeMoveReplace,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::AdvanceCrossVolumeMoveReplace(key, ledger, reply))
    }

    pub fn register_directory_move_replace_rename(
        &self,
        key: String,
        ledger: JournalDirectoryMoveReplaceRename,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::RegisterDirectoryMoveReplaceRename(key, ledger, reply)
        })
    }

    pub fn advance_directory_move_replace_rename(
        &self,
        key: String,
        ledger: JournalDirectoryMoveReplaceRename,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::AdvanceDirectoryMoveReplaceRename(key, ledger, reply)
        })
    }

    pub fn record_directory_move_replace_rename_cleanup_intent(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::DirectoryMoveReplaceRenameCleanupIntent(key, item, intent, reply)
        })
    }

    pub fn record_directory_move_replace_rename_cleanup_receipt(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::DirectoryMoveReplaceRenameCleanupReceipt(key, item, intent, reply)
        })
    }

    pub fn complete_directory_move_replace_rename_backup(
        &self,
        key: String,
        item: u32,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::CompleteDirectoryMoveReplaceRenameBackup(key, item, reply)
        })
    }

    pub fn register_copy_replace_cleanup(
        &self,
        key: String,
        ledger: JournalCopyReplaceCleanup,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::RegisterCopyReplaceCleanup(key, ledger, reply))
    }

    pub fn record_copy_replace_cleanup_intent(
        &self,
        key: String,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::CopyReplaceCleanupIntent(key, item, intent, reply))
    }

    pub fn record_copy_replace_cleanup_receipt(
        &self,
        key: String,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::CopyReplaceCleanupReceipt(key, item, intent, reply)
        })
    }

    pub fn complete_copy_replace_cleanup(
        &self,
        key: String,
        item: u32,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::CompleteCopyReplaceCleanup(key, item, reply))
    }

    pub fn record_directory_move_intent(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::DirectoryMoveIntent(key, item, intent, reply))
    }

    pub fn record_directory_move_receipt(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::DirectoryMoveReceipt(key, item, intent, reply))
    }

    pub fn record_directory_move_root_intent(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryRootIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::DirectoryMoveRootIntent(key, item, intent, reply))
    }

    pub fn record_directory_move_root_receipt(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryRootIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::DirectoryMoveRootReceipt(key, item, intent, reply))
    }

    /// Call only after held source-root deletion is durably and authoritatively
    /// complete under the accepted protocol. This records that fact; it does
    /// not perform filesystem I/O.
    pub fn complete_directory_move(
        &self,
        key: String,
        item: u32,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| WriterCommand::CompleteDirectoryMove(key, item, reply))
    }

    pub fn advance_directory_move_replace(
        &self,
        key: String,
        item: u32,
        phase: DirectoryMoveReplacePhase,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::AdvanceDirectoryMoveReplace(key, item, phase, reply)
        })
    }

    pub fn record_directory_move_replace_backup_intent(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryMoveReplaceChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::DirectoryMoveReplaceBackupIntent(key, item, intent, reply)
        })
    }

    pub fn record_directory_move_replace_backup_receipt(
        &self,
        key: String,
        item: u32,
        intent: JournalDirectoryMoveReplaceChildIntent,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::DirectoryMoveReplaceBackupReceipt(key, item, intent, reply)
        })
    }

    pub fn complete_directory_move_replace_backup(
        &self,
        key: String,
        item: u32,
    ) -> async_channel::Receiver<Result<(), String>> {
        self.acknowledged(|reply| {
            WriterCommand::CompleteDirectoryMoveReplaceBackup(key, item, reply)
        })
    }

    pub fn start(store: JournalStore) -> Self {
        #[cfg(test)]
        {
            Self::start_inner(store, None)
        }
        #[cfg(not(test))]
        {
            Self::start_inner(store)
        }
    }

    #[cfg(test)]
    pub(crate) fn start_observed_for_test(
        store: JournalStore,
    ) -> (Self, async_channel::Receiver<JournalWriterObservationForTest>) {
        let (observer, observations) = async_channel::unbounded();
        (Self::start_inner(store, Some(observer)), observations)
    }

    fn start_inner(
        mut store: JournalStore,
        #[cfg(test)] observer: Option<async_channel::Sender<JournalWriterObservationForTest>>,
    ) -> Self {
        let (sender, receiver) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("operation-journal-writer".into())
            .spawn(move || {
                while let Ok(command) = receiver.recv_blocking() {
                    match command {
                        WriterCommand::ReadAttempt(key, reply) => {
                            let result = Ok(store.attempt(&key).cloned());
                            #[cfg(test)]
                            if let Some(observer) = observer.as_ref() {
                                let _ = observer.try_send(JournalWriterObservationForTest::ReadAttempt {
                                    key,
                                    result: result.clone(),
                                });
                            }
                            let _ = reply.send_blocking(result);
                        }
                        WriterCommand::Upsert(attempt, reply) => {
                            #[cfg(test)]
                            gate_terminal_before_upsert_for_test(&attempt);
                            #[cfg(test)]
                            let observed_attempt = attempt.clone();
                            let result = store.upsert(attempt);
                            #[cfg(test)]
                            if let Some(observer) = observer.as_ref() {
                                let _ = observer.try_send(JournalWriterObservationForTest::Upsert {
                                    attempt: observed_attempt,
                                    result: result.clone(),
                                });
                            }
                            if let Some(reply) = reply {
                                let _ = reply.send_blocking(result);
                            }
                        }
                        WriterCommand::RegisterDirectoryMove(key, ledger, reply) => {
                            let _ =
                                reply.send_blocking(store.register_directory_move(&key, ledger));
                        }
                        WriterCommand::RegisterCrossVolumeMoveReplace(key, ledger, reply) => {
                            let _ = reply.send_blocking(
                                store.register_cross_volume_move_replace(&key, ledger),
                            );
                        }
                        WriterCommand::AdvanceCrossVolumeMoveReplace(key, ledger, reply) => {
                            let _ = reply.send_blocking(
                                store.advance_cross_volume_move_replace(&key, ledger),
                            );
                        }
                        WriterCommand::RegisterDirectoryMoveReplaceRename(key, ledger, reply) => {
                            let _ = reply.send_blocking(
                                store.register_directory_move_replace_rename(&key, ledger),
                            );
                        }
                        WriterCommand::AdvanceDirectoryMoveReplaceRename(key, ledger, reply) => {
                            let _ = reply.send_blocking(
                                store.advance_directory_move_replace_rename(&key, ledger),
                            );
                        }
                        WriterCommand::DirectoryMoveReplaceRenameCleanupIntent(
                            key, item, intent, reply,
                        ) => {
                            let _ = reply.send_blocking(
                                store.record_directory_move_replace_rename_cleanup_intent(
                                    &key, item, intent,
                                ),
                            );
                        }
                        WriterCommand::DirectoryMoveReplaceRenameCleanupReceipt(
                            key, item, intent, reply,
                        ) => {
                            let _ = reply.send_blocking(
                                store.record_directory_move_replace_rename_cleanup_receipt(
                                    &key, item, intent,
                                ),
                            );
                        }
                        WriterCommand::CompleteDirectoryMoveReplaceRenameBackup(
                            key, item, reply,
                        ) => {
                            let _ = reply.send_blocking(
                                store.complete_directory_move_replace_rename_backup(&key, item),
                            );
                        }
                        WriterCommand::RegisterCopyReplaceCleanup(key, ledger, reply) => {
                            let _ = reply
                                .send_blocking(store.register_copy_replace_cleanup(&key, ledger));
                        }
                        WriterCommand::CopyReplaceCleanupIntent(key, item, intent, reply) => {
                            let _ = reply.send_blocking(
                                store.record_copy_replace_cleanup_intent(&key, item, intent),
                            );
                        }
                        WriterCommand::CopyReplaceCleanupReceipt(key, item, intent, reply) => {
                            #[cfg(test)]
                            gate_copy_replace_root_receipt_before_store_for_test(
                                &store, &key, item, &intent,
                            );
                            let result = store.record_copy_replace_cleanup_receipt(&key, item, intent.clone());
                            #[cfg(test)]
                            gate_copy_replace_root_receipt_after_store_for_test(
                                &store, &key, item, &intent, &result,
                            );
                            let _ = reply.send_blocking(result);
                        }
                        WriterCommand::CompleteCopyReplaceCleanup(key, item, reply) => {
                            #[cfg(test)]
                            gate_copy_replace_cleanup_complete_before_store_for_test(
                                &store, &key, item,
                            );
                            let result = store.complete_copy_replace_cleanup(&key, item);
                            #[cfg(test)]
                            gate_copy_replace_cleanup_complete_after_store_for_test(
                                &store, &key, item, &result,
                            );
                            let _ = reply.send_blocking(result);
                        }
                        WriterCommand::DirectoryMoveIntent(key, item, intent, reply) => {
                            #[cfg(test)]
                            gate_directory_move_intent_before_store_for_test(&store, &key, item, &intent);
                            let _ = reply.send_blocking(
                                store.record_directory_move_intent(&key, item, intent),
                            );
                        }
                        WriterCommand::DirectoryMoveReceipt(key, item, intent, reply) => {
                            let _ = reply.send_blocking(
                                store.record_directory_move_receipt(&key, item, intent),
                            );
                        }
                        WriterCommand::DirectoryMoveRootIntent(key, item, intent, reply) => {
                            let result = store.record_directory_move_root_intent(&key, item, intent.clone());
                            #[cfg(test)]
                            gate_directory_move_root_intent_after_store_for_test(
                                &store, &key, item, &intent, &result,
                            );
                            let _ = reply.send_blocking(result);
                        }
                        WriterCommand::DirectoryMoveRootReceipt(key, item, intent, reply) => {
                            #[cfg(test)]
                            gate_directory_move_root_receipt_before_store_for_test(
                                &store, &key, item, &intent,
                            );
                            let _ = reply.send_blocking(
                                store.record_directory_move_root_receipt(&key, item, intent),
                            );
                        }
                        WriterCommand::CompleteDirectoryMove(key, item, reply) => {
                            let _ = reply.send_blocking(store.complete_directory_move(&key, item));
                        }
                        WriterCommand::AdvanceDirectoryMoveReplace(key, item, phase, reply) => {
                            let _ = reply.send_blocking(
                                store.advance_directory_move_replace(&key, item, phase),
                            );
                        }
                        WriterCommand::DirectoryMoveReplaceBackupIntent(
                            key,
                            item,
                            intent,
                            reply,
                        ) => {
                            let _ = reply.send_blocking(
                                store.record_directory_move_replace_backup_intent(
                                    &key, item, intent,
                                ),
                            );
                        }
                        WriterCommand::DirectoryMoveReplaceBackupReceipt(
                            key,
                            item,
                            intent,
                            reply,
                        ) => {
                            let _ = reply.send_blocking(
                                store.record_directory_move_replace_backup_receipt(
                                    &key, item, intent,
                                ),
                            );
                        }
                        WriterCommand::CompleteDirectoryMoveReplaceBackup(key, item, reply) => {
                            let _ = reply.send_blocking(
                                store.complete_directory_move_replace_backup(&key, item),
                            );
                        }
                        WriterCommand::Discard(key, reply) => {
                            let result = store.discard(&key);
                            let _ = reply.send_blocking(result);
                        }
                    }
                }
            })
            .expect("operation journal writer thread must start");
        Self { sender }
    }

    pub fn upsert(&self, attempt: JournalAttempt) -> Result<(), String> {
        self.sender
            .send_blocking(WriterCommand::Upsert(attempt, None))
            .map_err(|_| "operation journal writer is unavailable".to_string())
    }

    /// Reads an immutable snapshot through the same serial owner as journal writes.
    pub fn read_attempt(
        &self,
        key: String,
    ) -> async_channel::Receiver<Result<Option<JournalAttempt>, String>> {
        let (reply, receiver) = async_channel::bounded(1);
        if self
            .sender
            .send_blocking(WriterCommand::ReadAttempt(key, reply.clone()))
            .is_err()
        {
            let _ = reply.send_blocking(Err("operation journal writer is unavailable".into()));
        }
        receiver
    }

    pub fn upsert_with_reply(
        &self,
        attempt: JournalAttempt,
    ) -> async_channel::Receiver<Result<(), String>> {
        let (reply, receiver) = async_channel::bounded(1);
        if self
            .sender
            .send_blocking(WriterCommand::Upsert(attempt, Some(reply.clone())))
            .is_err()
        {
            let _ = reply.send_blocking(Err("operation journal writer is unavailable".into()));
        }
        receiver
    }

    pub fn discard(&self, key: String) -> async_channel::Receiver<Result<bool, String>> {
        let (reply, receiver) = async_channel::bounded(1);
        if self
            .sender
            .send_blocking(WriterCommand::Discard(key, reply.clone()))
            .is_err()
        {
            let _ = reply.send_blocking(Err("operation journal writer is unavailable".into()));
        }
        receiver
    }
}

impl JournalStore {
    pub fn attempt(&self, key: &str) -> Option<&JournalAttempt> {
        self.document.attempts.iter().find(|attempt| attempt.key == key)
    }
    /// `None` means only that app-data has no location. Corrupt and future
    /// journals reach the Shell as a diagnostic and are never overwritten.
    pub fn open_for_app() -> Result<Option<Self>, String> {
        let Some(root) = crate::app_paths::root() else {
            return Ok(None);
        };
        Self::open(root.join("operations").join("journal-v1.json")).map(Some)
    }

    pub fn default_for_app() -> Option<Self> {
        Self::open_for_app().ok().flatten()
    }

    pub fn open(path: PathBuf) -> Result<Self, String> {
        let file = SessionFile::new(SessionPaths::new(path));
        Self::open_with_file(file)
    }

    #[cfg(test)]
    pub(crate) fn open_with_partial_write_fault(
        path: PathBuf,
        fault: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self, String> {
        let file = SessionFile::new_with_partial_write_fault(SessionPaths::new(path), fault);
        Self::open_with_file(file)
    }

    fn open_with_file(file: SessionFile) -> Result<Self, String> {
        let document = match file.read_primary()? {
            None => JournalDocument {
                schema: SCHEMA,
                attempts: Vec::new(),
            },
            Some(bytes) => decode(&bytes)?,
        };
        Ok(Self { file, document })
    }

    pub fn recoveries(&self) -> Vec<RecoveryItem> {
        self.document
            .attempts
            .iter()
            .filter_map(|attempt| {
                let recovery_required = if let Some(ledger) = &attempt.directory_move_replace_rename {
                    attempt.state != JournalState::Terminal
                        || match ledger.phase {
                            DirectoryMoveReplaceRenamePhase::Complete => {
                                attempt.terminal_outcome != Some(JournalTerminalOutcome::Succeeded)
                            }
                            DirectoryMoveReplaceRenamePhase::NoEffect => !matches!(
                                attempt.terminal_outcome,
                                Some(JournalTerminalOutcome::Failed | JournalTerminalOutcome::Cancelled)
                            ),
                            _ => true,
                        }
                } else if let Some(ledger) = &attempt.cross_volume_move_replace {
                    !matches!(attempt.state, JournalState::Terminal)
                        || match ledger.phase {
                            CrossVolumeMoveReplacePhase::Complete => {
                                attempt.terminal_outcome != Some(JournalTerminalOutcome::Succeeded)
                            }
                            CrossVolumeMoveReplacePhase::StageNotCreated
                            | CrossVolumeMoveReplacePhase::StageDiscarded => !matches!(
                                attempt.terminal_outcome,
                                Some(JournalTerminalOutcome::Failed | JournalTerminalOutcome::Cancelled)
                            ),
                            _ => true,
                        }
                } else if !attempt.directory_moves.is_empty() {
                    !matches!(attempt.state, JournalState::Terminal)
                        || !matches!(
                            attempt.terminal_outcome,
                            Some(JournalTerminalOutcome::Succeeded)
                        )
                        || attempt.directory_moves.iter().any(|directory_move| {
                            !directory_move.cleanup_complete
                                || directory_move.pending.is_some()
                                || directory_move.root.as_ref().is_none_or(|root| {
                                    root.pending.is_some() || !root.disposition_recorded
                                })
                        })
                } else if !attempt.copy_replace_cleanups.is_empty() {
                    !matches!(attempt.state, JournalState::Terminal)
                        || !matches!(
                            attempt.terminal_outcome,
                            Some(JournalTerminalOutcome::Succeeded)
                        )
                        || attempt.copy_replace_cleanups.iter().any(|ledger| {
                            !ledger.cleanup_complete
                                || ledger.pending.is_some()
                                || !ledger.root_disposition_recorded
                        })
                } else if let Some(ledger) = &attempt.zip_create {
                    !matches!(attempt.state, JournalState::Terminal)
                        || match ledger.phase {
                            JournalZipCreatePhase::Published => {
                                attempt.terminal_outcome != Some(JournalTerminalOutcome::Succeeded)
                            }
                            JournalZipCreatePhase::StageDiscarded => !matches!(
                                attempt.terminal_outcome,
                                Some(
                                    JournalTerminalOutcome::Cancelled
                                        | JournalTerminalOutcome::Failed
                                )
                            ),
                            _ => true,
                        }
                } else if let Some(ledger) = &attempt.seven_zip_create_handles {
                    match ledger.phase {
                        JournalSevenZipHandlePhase::Published => {
                            attempt.state != JournalState::Terminal
                                || attempt.terminal_outcome
                                    != Some(JournalTerminalOutcome::Succeeded)
                                || !attempt.receipts_recorded
                                || attempt.created_archive_path.as_ref() != Some(&ledger.output)
                        }
                        JournalSevenZipHandlePhase::Discarded => {
                            attempt.state != JournalState::Terminal
                                || !matches!(
                                    attempt.terminal_outcome,
                                    Some(
                                        JournalTerminalOutcome::Cancelled
                                            | JournalTerminalOutcome::Failed
                                    )
                                )
                                || !attempt.receipts_recorded
                        }
                        _ => true,
                    }
                } else if let Some(ledger) = &attempt.seven_zip_create {
                    match ledger.phase {
                        JournalSevenZipCreatePhase::Published => {
                            !matches!(attempt.state, JournalState::Terminal)
                                || attempt.terminal_outcome
                                    != Some(JournalTerminalOutcome::Succeeded)
                                || !attempt.receipts_recorded
                        }
                        JournalSevenZipCreatePhase::Discarded => {
                            !matches!(attempt.state, JournalState::Terminal)
                                || !matches!(
                                    attempt.terminal_outcome,
                                    Some(
                                        JournalTerminalOutcome::Cancelled
                                            | JournalTerminalOutcome::Failed
                                    )
                                )
                                || !attempt.receipts_recorded
                        }
                        _ => true,
                    }
                } else if let Some(ledger) = &attempt.zip_extract {
                    !matches!(attempt.state, JournalState::Terminal)
                        || match ledger.phase {
                            JournalZipExtractPhase::Published => {
                                attempt.terminal_outcome != Some(JournalTerminalOutcome::Succeeded)
                            }
                            JournalZipExtractPhase::StageDiscarded => !matches!(
                                attempt.terminal_outcome,
                                Some(
                                    JournalTerminalOutcome::Cancelled
                                        | JournalTerminalOutcome::Failed
                                )
                            ),
                            _ => true,
                        }
                } else if attempt.seven_zip_extract_batch.is_some() || !attempt.seven_zip_extract_receipts.is_empty() {
                    attempt.state != JournalState::Terminal
                        || !attempt.receipts_recorded
                        || attempt.seven_zip_extract_receipts.iter().any(|receipt| matches!(
                            receipt.outcome,
                            JournalSevenZipExtractOutcome::Partial | JournalSevenZipExtractOutcome::Unknown
                        ))
                } else if !attempt.zip_extract_items.is_empty() {
                    !matches!(attempt.state, JournalState::Terminal)
                        || !attempt.receipts_recorded
                        || attempt.zip_extract_items.iter().any(|entry| {
                            let receipt = attempt
                                .zip_extract_receipts
                                .iter()
                                .find(|receipt| receipt.item == entry.item);
                            match (entry.ledger.phase, receipt) {
                                (JournalZipExtractPhase::Published, Some(receipt)) => {
                                    receipt.outcome != JournalZipExtractItemOutcome::Completed
                                        || receipt.output.as_ref() != Some(&entry.ledger.output)
                                }
                                (JournalZipExtractPhase::StageDiscarded, Some(receipt)) => {
                                    !matches!(
                                        receipt.outcome,
                                        JournalZipExtractItemOutcome::Failed
                                            | JournalZipExtractItemOutcome::CancelledBeforeStart
                                    ) || receipt.output.is_some()
                                }
                                _ => true,
                            }
                        })
                } else {
                    !matches!(attempt.state, JournalState::Terminal)
                        || attempt.replace_backup.is_some()
                        || attempt.rename_stage.is_some()
                        || (matches!(
                            attempt.kind,
                            JournalKind::BatchRename | JournalKind::BatchRenameUndo
                        ) && matches!(
                            attempt.terminal_outcome,
                            Some(JournalTerminalOutcome::Partial | JournalTerminalOutcome::Unknown)
                        ))
                        || (attempt.kind == JournalKind::Copy
                            && attempt.copy_directory_stage.is_some()
                            && attempt.terminal_outcome == Some(JournalTerminalOutcome::Unknown))
                };
                recovery_required.then(|| {
                    let mut attempt = attempt.clone();
                    attempt.state = JournalState::RecoveryRequired;
                    RecoveryItem::RecoveryRequired(attempt)
                })
            })
            .collect()
    }

    pub fn attempt_keys(&self) -> Vec<String> {
        self.document
            .attempts
            .iter()
            .map(|attempt| attempt.key.clone())
            .collect()
    }

    pub fn upsert(&mut self, mut attempt: JournalAttempt) -> Result<(), String> {
        // Preserve whether this snapshot supplied the Move Replace ledger itself;
        // omission must not be repaired into authority for a recovery transition.
        let supplied_cross_volume_move_replace = attempt.cross_volume_move_replace.clone();
        if let Some(previous) = self
            .document
            .attempts
            .iter()
            .find(|previous| previous.key == attempt.key)
        {
            // 古い7z記録は新しいスナップショットから省略されても保存し続ける。
            if attempt.seven_zip_create.is_none() {
                attempt.seven_zip_create = previous.seven_zip_create.clone();
            }
            if attempt.seven_zip_create_handles.is_none() {
                attempt.seven_zip_create_handles = previous.seven_zip_create_handles.clone();
            }
            if attempt.seven_zip_extract_batch.is_none() {
                attempt.seven_zip_extract_batch = previous.seven_zip_extract_batch.clone();
            }
            if attempt.seven_zip_extract_receipts.is_empty() {
                attempt.seven_zip_extract_receipts = previous.seven_zip_extract_receipts.clone();
            }
            if attempt.copy_directory_publications.is_empty() {
                attempt.copy_directory_publications = previous.copy_directory_publications.clone();
            }
        }
        let previous_attempt = self
            .document
            .attempts
            .iter()
            .find(|previous| previous.key == attempt.key);
        if previous_attempt.is_none() && attempt.directory_move_replace_rename.is_some() {
            return Err(
                "same-volume directory Move Replace must be introduced by its registration API"
                    .into(),
            );
        }
        if attempt.seven_zip_extract_batch.as_ref().is_some_and(|batch| !batch.is_registration())
            && previous_attempt.and_then(|previous| previous.seven_zip_extract_batch.as_ref()).is_none()
        {
            return Err("7z extraction batch must begin with an empty sequence-one registration".into());
        }
        if let Some(previous) = previous_attempt {
            if let Some(existing) = &previous.seven_zip_extract_batch {
                match &attempt.seven_zip_extract_batch {
                    None => unreachable!("omitted batch is restored above"),
                    Some(candidate) if candidate == existing => {}
                    Some(candidate) if candidate.follows(existing) => {}
                    Some(_) => return Err("7z extraction batch must retain its envelope and exact barrier sequence".into()),
                }
            } else if attempt.seven_zip_extract_batch.as_ref().is_some_and(|batch| !batch.is_registration()) {
                return Err("7z extraction batch must register before item intent".into());
            }
            if !previous.seven_zip_extract_receipts.is_empty()
                && previous.seven_zip_extract_receipts != attempt.seven_zip_extract_receipts
            {
                return Err("7z extraction terminal receipts are immutable".into());
            }
            if previous.zip_extract_items.iter().any(|old| {
                attempt
                    .zip_extract_items
                    .iter()
                    .find(|candidate| candidate.item == old.item)
                    .is_none()
            }) {
                return Err("ZIP extract item ledgers cannot be erased".into());
            }
            for candidate in &attempt.zip_extract_items {
                if let Some(old) = previous
                    .zip_extract_items
                    .iter()
                    .find(|old| old.item == candidate.item)
                {
                    if old.ledger != candidate.ledger
                        && !zip_extract_transition_is_valid(&old.ledger, &candidate.ledger)
                    {
                        return Err(
                            "ZIP extract item ledger changes require an exact phase transition"
                                .into(),
                        );
                    }
                } else if previous.kind != JournalKind::ExtractZip
                    || previous.zip_extract.is_some()
                    || candidate.item as usize >= previous.sources.len()
                    || previous.sources[candidate.item as usize] != candidate.ledger.source
                    || candidate.ledger.phase != JournalZipExtractPhase::StagePlanned
                    || candidate.ledger.stage_identity.is_some()
                    || candidate.ledger.source_identity.is_empty()
                    || candidate.ledger.parent_identity.is_empty()
                {
                    return Err("invalid initial ZIP extract item ledger".into());
                }
            }
            if previous.zip_extract_items.is_empty()
                && !attempt.zip_extract_items.is_empty()
                && previous.zip_extract.is_some()
            {
                return Err("singleton and batch ZIP extract ledgers cannot be combined".into());
            }
            for old in &previous.zip_extract_items {
                if let Some(candidate) = attempt
                    .zip_extract_items
                    .iter()
                    .find(|candidate| candidate.item == old.item)
                {
                    if candidate.ledger != old.ledger
                        && candidate.ledger.phase == JournalZipExtractPhase::StageDiscarded
                        && candidate.ledger.stage_identity == old.ledger.stage_identity
                        && !zip_extract_transition_is_valid(&old.ledger, &candidate.ledger)
                    {
                        return Err("invalid ZIP extract cancellation cleanup transition".into());
                    }
                }
            }
            if let Some(existing) = &previous.cross_volume_move_replace {
                match &attempt.cross_volume_move_replace {
                    None => attempt.cross_volume_move_replace = Some(existing.clone()),
                    Some(candidate) if candidate == existing => {}
                    Some(_) => {
                        return Err(
                            "cross-volume Move Replace ledger changes require a phase transition"
                                .into(),
                        );
                    }
                }
            }
            if previous.cross_volume_move_replace.is_none()
                && attempt.cross_volume_move_replace.is_some()
            {
                return Err("cross-volume Move Replace must enter through registration".into());
            }
            if let Some(existing) = &previous.directory_move_replace_rename {
                match &attempt.directory_move_replace_rename {
                    None => attempt.directory_move_replace_rename = Some(existing.clone()),
                    Some(candidate) if candidate == existing => {}
                    Some(_) => {
                        return Err(
                            "same-volume directory Move Replace changes require its transition API"
                                .into(),
                        );
                    }
                }
            }
            if previous.directory_move_replace_rename.is_none()
                && attempt.directory_move_replace_rename.is_some()
            {
                return Err(
                    "same-volume directory Move Replace must enter through registration".into(),
                );
            }
            if let Some(existing) = &previous.zip_create {
                match &attempt.zip_create {
                    None => attempt.zip_create = Some(existing.clone()),
                    Some(candidate) if candidate == existing => {}
                    Some(candidate) if zip_create_transition_is_valid(existing, candidate) => {}
                    Some(_) => {
                        return Err(
                            "ZIP create ledger changes require an exact phase transition".into(),
                        );
                    }
                }
            }
            if let Some(existing) = &previous.zip_extract {
                match &attempt.zip_extract {
                    None => attempt.zip_extract = Some(existing.clone()),
                    Some(candidate) if candidate == existing => {}
                    Some(candidate) if zip_extract_transition_is_valid(existing, candidate) => {}
                    Some(_) => {
                        return Err(
                            "ZIP extract ledger changes require an exact phase transition".into(),
                        );
                    }
                }
            }
            if !previous.zip_extract_receipts.is_empty()
                && previous.zip_extract_receipts != attempt.zip_extract_receipts
            {
                return Err("ZIP extract item receipts are immutable".into());
            }
            if previous.zip_create.is_none() && attempt.zip_create.is_some() {
                let ledger = attempt.zip_create.as_ref().expect("checked above");
                if previous.kind != JournalKind::CreateZip
                    || previous.sources
                        != ledger
                            .sources
                            .iter()
                            .map(|source| source.source.clone())
                            .collect::<Vec<_>>()
                    || previous.destination.as_ref() != Some(&ledger.output)
                    || ledger.phase != JournalZipCreatePhase::StagePlanned
                    || ledger.stage_identity.is_some()
                {
                    return Err("invalid initial ZIP create ledger".into());
                }
            }
            if previous.zip_extract.is_none() && attempt.zip_extract.is_some() {
                let ledger = attempt.zip_extract.as_ref().expect("checked above");
                if previous.kind != JournalKind::ExtractZip
                    || previous.sources.as_slice() != [ledger.source.clone()]
                    || previous.destination.as_ref() != Some(&ledger.output)
                    || ledger.phase != JournalZipExtractPhase::StagePlanned
                    || ledger.stage_identity.is_some()
                {
                    return Err("invalid initial ZIP extract ledger".into());
                }
            }
            let cancelled_zip_extract_cleanup = previous.state == JournalState::CancelRequested
                && attempt.state == JournalState::CancelRequested
                && previous.kind == JournalKind::ExtractZip
                && previous.zip_extract.as_ref().is_some_and(|existing| {
                    attempt.zip_extract.as_ref().is_some_and(|candidate| {
                        candidate.phase == JournalZipExtractPhase::StageDiscarded
                            && candidate.stage_identity == existing.stage_identity
                            && zip_extract_transition_is_valid(existing, candidate)
                    })
                })
                && {
                    let mut expected = previous.clone();
                    expected.zip_extract = attempt.zip_extract.clone();
                    expected == attempt
                };
            let cancelled_zip_extract_item_cleanup = previous.state
                == JournalState::CancelRequested
                && attempt.state == JournalState::CancelRequested
                && previous.kind == JournalKind::ExtractZip
                && !previous.zip_extract_items.is_empty()
                && previous.zip_extract_items.iter().any(|existing| {
                    attempt.zip_extract_items.iter().any(|candidate| {
                        candidate.item == existing.item
                            && candidate.ledger.phase == JournalZipExtractPhase::StageDiscarded
                            && candidate.ledger.stage_identity == existing.ledger.stage_identity
                            && zip_extract_transition_is_valid(&existing.ledger, &candidate.ledger)
                    })
                })
                && {
                    let mut expected = previous.clone();
                    expected.zip_extract_items = attempt.zip_extract_items.clone();
                    expected == attempt
                };
            let cancelled_zip_extract_item_initial = previous.state
                == JournalState::CancelRequested
                && attempt.state == JournalState::CancelRequested
                && previous.kind == JournalKind::ExtractZip
                && previous.zip_extract_items.is_empty()
                && previous.zip_extract.is_none()
                && attempt.zip_extract_items.len() == 1
                && attempt.zip_extract_items[0].ledger.phase
                    == JournalZipExtractPhase::StagePlanned
                && attempt.zip_extract_items[0].ledger.stage_identity.is_none()
                && {
                    let mut expected = previous.clone();
                    expected.zip_extract_items = attempt.zip_extract_items.clone();
                    expected == attempt
                };
            let cancelled_seven_zip_extract_observation = previous.state == JournalState::CancelRequested
                && attempt.state == JournalState::CancelRequested
                && previous.kind == JournalKind::Extract7z
                && previous.seven_zip_extract_batch.as_ref().zip(attempt.seven_zip_extract_batch.as_ref()).is_some_and(|(old, next)| {
                    old == next || next.is_observation_successor_of(old) || next.is_cancel_cleanup_successor_of(old)
                })
                && {
                    let mut expected = previous.clone();
                    expected.seven_zip_extract_batch = attempt.seven_zip_extract_batch.clone();
                    expected == attempt
                };
        let cancelled_zip_create_cleanup = previous.state == JournalState::CancelRequested
                && attempt.state == JournalState::CancelRequested
                && previous.kind == JournalKind::CreateZip
                && previous.zip_create.as_ref().is_some_and(|existing| {
                    attempt.zip_create.as_ref().is_some_and(|candidate| {
                        candidate.phase == JournalZipCreatePhase::StageDiscarded
                            && candidate
                                .stage_identity
                                .as_deref()
                                .is_none_or(|identity| !identity.is_empty())
                            && zip_create_transition_is_valid(existing, candidate)
                    })
                })
                && {
                    let mut expected = previous.clone();
                    expected.zip_create = attempt.zip_create.clone();
                    expected == attempt
                };
            let cancelled_seven_zip_extract_terminal_recovery =
                previous.state == JournalState::CancelRequested
                    && attempt.state == JournalState::RecoveryRequired
                    && attempt.kind == JournalKind::Extract7z
                    && matches!(attempt.terminal_outcome,
                        Some(JournalTerminalOutcome::Partial | JournalTerminalOutcome::Unknown))
                    && validate_seven_zip_extract_batch(&attempt).is_ok();
            let cancelled_cross_volume_move_replace_terminal_recovery =
                previous.state == JournalState::CancelRequested
                    && previous.terminal_outcome.is_none()
                    && attempt.state == JournalState::RecoveryRequired
                    && previous.kind == JournalKind::Move
                    && attempt.kind == JournalKind::Move
                    && previous.sources.len() == 1
                    && !previous.sources[0].units.is_empty()
                    && attempt.sources == previous.sources
                    && previous.cross_volume_move_replace.as_ref().is_some_and(|existing| {
                        existing.phase == CrossVolumeMoveReplacePhase::StageReady
                            && !existing.stage_identity.is_empty()
                            && supplied_cross_volume_move_replace.as_ref() == Some(existing)
                            && attempt.cross_volume_move_replace.as_ref() == Some(existing)
                    })
                    && matches!(
                        attempt.terminal_outcome,
                        Some(JournalTerminalOutcome::Partial | JournalTerminalOutcome::Unknown)
                    )
                    && attempt.receipts_recorded
                    && validate_cross_volume_move_replace(&attempt).is_ok()
                    && {
                        let mut expected = previous.clone();
                        expected.state = attempt.state;
                        expected.terminal_outcome = attempt.terminal_outcome;
                        expected.receipts_recorded = attempt.receipts_recorded;
                        expected == attempt
                    };
            if previous.state == JournalState::CancelRequested
                && attempt.state != JournalState::Terminal
                && !cancelled_seven_zip_extract_terminal_recovery
                && !cancelled_cross_volume_move_replace_terminal_recovery
                && !cancelled_zip_extract_cleanup
                && !cancelled_zip_extract_item_cleanup
                && !cancelled_zip_extract_item_initial
                && !cancelled_seven_zip_extract_observation
                && !cancelled_zip_create_cleanup
            {
                return Err(
                    "cancel-requested journal attempt cannot return to a live state".into(),
                );
            }
            if previous.state == JournalState::RecoveryRequired
                && attempt.state != JournalState::RecoveryRequired
            {
                return Err(
                    "recovery-required journal attempt cannot resume from a snapshot".into(),
                );
            }
            if previous.state == JournalState::Terminal && attempt.state != JournalState::Terminal {
                return Err(
                    "terminal journal attempt cannot be overwritten by a nonterminal snapshot"
                        .into(),
                );
            }
            attempt.receipts_recorded |= previous.receipts_recorded;
        }
        let existing_ledgers = previous_attempt
            .map(|previous| previous.directory_moves.clone())
            .unwrap_or_default();
        if attempt.directory_moves.is_empty() {
            attempt.directory_moves = existing_ledgers.clone();
        } else if attempt.directory_moves != existing_ledgers {
            return Err(
                "directory move ledgers may only change through their transition API".into(),
            );
        }
        let existing_copy_cleanups = previous_attempt
            .map(|previous| previous.copy_replace_cleanups.clone())
            .unwrap_or_default();
        if attempt.copy_replace_cleanups.is_empty() {
            attempt.copy_replace_cleanups = existing_copy_cleanups.clone();
        } else if attempt.copy_replace_cleanups != existing_copy_cleanups {
            return Err(
                "Copy Replace cleanup ledgers may only change through their transition API".into(),
            );
        }
        validate_directory_move_vector(&attempt)?;
        validate_copy_replace_cleanup_vector(&attempt)?;
        validate_copy_directory_publication_evidence(&attempt)?;
        validate_copy_directory_stage_evidence(&attempt)?;
        if let Some(previous) = previous_attempt
            && !previous.copy_directory_publications.is_empty()
            && attempt.copy_directory_publications != previous.copy_directory_publications
        {
            return Err("directory Copy publication evidence is immutable once recorded".into());
        }
        validate_cross_volume_move_replace(&attempt)?;
        validate_zip_create(&attempt)?;
        validate_seven_zip_create(&attempt)?;
        validate_seven_zip_create_handles(&attempt)?;
        validate_seven_zip_extract_batch(&attempt)?;
        if previous_attempt.is_some_and(|previous| {
            previous
                .seven_zip_create
                .as_ref()
                .is_some_and(|old| Some(old) != attempt.seven_zip_create.as_ref())
        }) {
            return Err("legacy 7z ledger is frozen for inspection".into());
        }
        if let Some(previous) = previous_attempt
            && previous
                .seven_zip_create_handles
                .as_ref()
                .is_some_and(|old| {
                    attempt
                        .seven_zip_create_handles
                        .as_ref()
                        .is_none_or(|new| !new.follows(old))
                })
        {
            return Err("handle-based 7z facts must follow the exact prior sequence".into());
        }
        if let Some(previous) = previous_attempt
            && previous.seven_zip_create_handles.is_none()
            && attempt
                .seven_zip_create_handles
                .as_ref()
                .is_some_and(|ledger| {
                    ledger.sequence != 1
                        || ledger.phase != JournalSevenZipHandlePhase::CandidateCreateIntent
                        || ledger.aborting
                        || ledger.candidate.state != JournalSevenZipHandleObjectState::CreateIntent
                        || ledger.stage.state != JournalSevenZipHandleObjectState::Planned
                })
        {
            return Err(
                "initial handle-based 7z ledger must establish the candidate create intent".into(),
            );
        }
        if previous_attempt.is_some_and(|previous| previous.seven_zip_create.is_some())
            && attempt.seven_zip_create
                != previous_attempt.and_then(|previous| previous.seven_zip_create.clone())
        {
            return Err("legacy 7z ledger is frozen after schema migration".into());
        }
        if attempt.seven_zip_create.is_some()
            && previous_attempt.is_none_or(|previous| previous.seven_zip_create.is_none())
        {
            return Err("legacy 7z ledger cannot be created by schema 8 writers".into());
        }
        validate_zip_extract(&attempt)?;
        if let Some(previous) = previous_attempt {
            if previous.state == JournalState::Terminal && previous != &attempt {
                return Err("conflicting terminal journal snapshot".into());
            }
        }
        if let Some(previous) = previous_attempt {
            if previous.seven_zip_extract_batch.is_none()
                && attempt.seven_zip_extract_batch.is_some()
                && !attempt.seven_zip_extract_batch.as_ref().is_some_and(JournalSevenZipExtractBatch::is_registration)
            {
                return Err("7z extraction batch registration is not the initial barrier".into());
            }
            if previous.state == JournalState::CancelRequested && attempt.state == JournalState::Running {
                return Err("cancel-requested operation cannot return to running".into());
            }
        }
        let previous_document = self.document.clone();
        if let Some(previous) = self
            .document
            .attempts
            .iter_mut()
            .find(|previous| previous.key == attempt.key)
        {
            *previous = attempt;
        } else {
            self.document.attempts.push(attempt);
        }
        if let Err(error) = self.flush() {
            self.document = previous_document;
            return Err(error);
        }
        Ok(())
    }

    fn register_cross_volume_move_replace(
        &mut self,
        key: &str,
        ledger: JournalCrossVolumeMoveReplace,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale cross-volume Move Replace attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        if let Some(existing) = &attempt.cross_volume_move_replace {
            return if existing == &ledger {
                Ok(())
            } else {
                Err("conflicting cross-volume Move Replace registration".into())
            };
        }
        if attempt.kind != JournalKind::Move
            || !matches!(
                attempt.state,
                JournalState::Preparing | JournalState::Running | JournalState::CancelRequested
            )
            || attempt.sources.len() != 1
            || ledger.item != 0
            || attempt.sources.first() != Some(&ledger.source)
            || !matches!(ledger.phase,
                CrossVolumeMoveReplacePhase::StageCreateIntent | CrossVolumeMoveReplacePhase::StageReady)
            || (ledger.phase == CrossVolumeMoveReplacePhase::StageReady
                && attempt.replace_backup.as_ref().is_none_or(|backup| {
                    backup.source != ledger.source || backup.destination != ledger.destination || backup.backup != ledger.backup
                }))
        {
            return Err("invalid cross-volume Move Replace registration".into());
        }
        let previous = self.document.clone();
        let previous_backup = self.document.attempts[index].replace_backup.clone();
        if ledger.phase == CrossVolumeMoveReplacePhase::StageCreateIntent {
            let reservation = JournalReplaceBackup { source: ledger.source.clone(), destination: ledger.destination.clone(), backup: ledger.backup.clone() };
            if previous_backup.as_ref().is_some_and(|backup| backup != &reservation) {
                return Err("conflicting cross-volume Move Replace backup reservation".into());
            }
            self.document.attempts[index].replace_backup = Some(reservation);
        }
        self.document.attempts[index].cross_volume_move_replace = Some(ledger);
        if let Err(error) = validate_cross_volume_move_replace(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn advance_cross_volume_move_replace(
        &mut self,
        key: &str,
        ledger: JournalCrossVolumeMoveReplace,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale cross-volume Move Replace attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        if attempt.kind != JournalKind::Move
            || attempt.sources.len() != 1
            || ledger.item != 0
            || !(attempt.state == JournalState::Running
                || (attempt.state == JournalState::CancelRequested
                && matches!(ledger.phase, CrossVolumeMoveReplacePhase::StageDiscarded | CrossVolumeMoveReplacePhase::StageNotCreated)))
        {
            return Err("invalid cross-volume Move Replace transition owner".into());
        }
        let previous_ledger = attempt
            .cross_volume_move_replace
            .as_ref()
            .ok_or_else(|| "cross-volume Move Replace is not registered".to_string())?;
        if !cross_volume_move_replace_same_identity(previous_ledger, &ledger)
            || (previous_ledger.phase != ledger.phase
                && !cross_volume_phase_next(previous_ledger.phase, ledger.phase))
            || (!previous_ledger.stage_identity.is_empty() && previous_ledger.stage_identity != ledger.stage_identity)
            || (!previous_ledger.content_sha256.is_empty() && previous_ledger.content_sha256 != ledger.content_sha256)
        {
            return Err(
                "cross-volume Move Replace accepts only exact duplicate or adjacent phase".into(),
            );
        }
        if previous_ledger == &ledger {
            return Ok(());
        }
        let previous = self.document.clone();
        self.document.attempts[index].cross_volume_move_replace = Some(ledger);
        if let Err(error) = validate_cross_volume_move_replace(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn register_directory_move_replace_rename(
        &mut self,
        key: &str,
        ledger: JournalDirectoryMoveReplaceRename,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale directory Move Replace rename attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        if let Some(existing) = &attempt.directory_move_replace_rename {
            return if existing == &ledger {
                Ok(())
            } else {
                Err("conflicting directory Move Replace rename registration".into())
            };
        }
        if attempt.kind != JournalKind::Move
            || !matches!(attempt.state, JournalState::Preparing | JournalState::Running)
            || attempt.sources.len() != 1
            || attempt.directory_moves.len() != 0
            || attempt.cross_volume_move_replace.is_some()
            || attempt.replace_backup.is_some()
            || ledger.item != 0
            || ledger.phase != DirectoryMoveReplaceRenamePhase::Prepared
            || ledger.pending_cleanup.is_some()
            || ledger.backup_root_disposed
            || ledger.approved_target_children.iter().any(|child| child.disposition_recorded)
        {
            return Err("invalid same-volume directory Move Replace rename registration".into());
        }
        let previous = self.document.clone();
        self.document.attempts[index].directory_move_replace_rename = Some(ledger);
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn advance_directory_move_replace_rename(
        &mut self,
        key: &str,
        ledger: JournalDirectoryMoveReplaceRename,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale directory Move Replace rename attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        let current = attempt
            .directory_move_replace_rename
            .as_ref()
            .ok_or_else(|| "directory Move Replace rename is not registered".to_string())?;
        let no_effect_after_cancel = attempt.state == JournalState::CancelRequested
            && ledger.phase == DirectoryMoveReplaceRenamePhase::NoEffect;
        if attempt.kind != JournalKind::Move
            || !(attempt.state == JournalState::Running || no_effect_after_cancel)
            || !directory_move_replace_rename_same_identity(current, &ledger)
            || (current.phase != ledger.phase
                && !directory_move_replace_rename_phase_next(current.phase, ledger.phase))
        {
            return Err("invalid directory Move Replace rename phase transition owner".into());
        }
        if current == &ledger {
            return Ok(());
        }
        let mut expected = current.clone();
        expected.phase = ledger.phase;
        expected.source_location = ledger.source_location;
        expected.target_location = ledger.target_location;
        if expected != ledger {
            return Err("directory Move Replace phase advance changed non-phase facts".into());
        }
        self.transition_directory_move_replace_rename(index, |attempt| {
            attempt.directory_move_replace_rename = Some(ledger);
        })
    }

    fn record_directory_move_replace_rename_cleanup_intent(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_replace_rename_index(key, item)?;
        let ledger = self.document.attempts[index]
            .directory_move_replace_rename.as_ref().unwrap();
        if self.document.attempts[index].state != JournalState::Running
            || ledger.phase != DirectoryMoveReplaceRenamePhase::BackupCleanup
            || ledger.pending_cleanup.is_some()
            || ledger.backup_root_disposed
        {
            return Err("directory Move Replace rename cleanup is not ready for an intent".into());
        }
        let is_root_intent = intent.ordinal as usize == ledger.approved_target_children.len()
            && intent.node_kind == JournalDirectoryNodeKind::Directory
            && intent.relative_components.is_empty()
            && intent.identity == ledger.approved_target_identity
            && intent.content_fact == "root"
            && ledger.approved_target_children.iter().all(|child| child.disposition_recorded);
        if !is_root_intent {
            let child = ledger.approved_target_children.get(intent.ordinal as usize)
                .ok_or_else(|| "directory Move Replace rename cleanup ordinal is invalid".to_string())?;
            if child.disposition_recorded
                || child.ordinal != intent.ordinal
                || child.node_kind != intent.node_kind
                || child.relative_components != intent.relative_components
                || child.identity != intent.identity
                || child.content_fact != intent.content_fact
            {
                return Err("directory Move Replace rename cleanup intent is not the next exact manifest entry".into());
            }
        }
        self.transition_directory_move_replace_rename(index, |attempt| {
            attempt.directory_move_replace_rename.as_mut().unwrap().pending_cleanup = Some(intent);
        })
    }

    fn record_directory_move_replace_rename_cleanup_receipt(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryMoveReplaceRenameChildIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_replace_rename_index(key, item)?;
        let ledger = self.document.attempts[index]
            .directory_move_replace_rename.as_ref().unwrap();
        if self.document.attempts[index].state != JournalState::Running
            || ledger.phase != DirectoryMoveReplaceRenamePhase::BackupCleanup
            || ledger.pending_cleanup.as_ref() != Some(&intent)
        {
            return Err("directory Move Replace rename cleanup receipt has no exact pending intent".into());
        }
        self.transition_directory_move_replace_rename(index, |attempt| {
            let ledger = attempt.directory_move_replace_rename.as_mut().unwrap();
            if intent.ordinal as usize == ledger.approved_target_children.len() {
                ledger.backup_root_disposed = true;
                ledger.target_location = DirectoryMoveReplaceTargetLocation::Disposed;
            } else {
                ledger.approved_target_children[intent.ordinal as usize].disposition_recorded = true;
            }
            ledger.pending_cleanup = None;
        })
    }

    fn complete_directory_move_replace_rename_backup(
        &mut self,
        key: &str,
        item: u32,
    ) -> Result<(), String> {
        let index = self.directory_move_replace_rename_index(key, item)?;
        let attempt = &self.document.attempts[index];
        let ledger = attempt.directory_move_replace_rename.as_ref().unwrap();
        if attempt.state != JournalState::Running
            || ledger.phase != DirectoryMoveReplaceRenamePhase::BackupCleanup
            || ledger.pending_cleanup.is_some()
            || !ledger.backup_root_disposed
            || ledger.approved_target_children.iter().any(|child| !child.disposition_recorded)
        {
            return Err("directory Move Replace rename backup cleanup is not complete".into());
        }
        self.transition_directory_move_replace_rename(index, |attempt| {
            attempt.directory_move_replace_rename.as_mut().unwrap().phase =
                DirectoryMoveReplaceRenamePhase::BackupComplete;
        })
    }

    fn directory_move_replace_rename_index(&self, key: &str, item: u32) -> Result<usize, String> {
        let index = self.document.attempts.iter().position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale directory Move Replace rename attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        if attempt.kind != JournalKind::Move
            || attempt.directory_move_replace_rename.as_ref().is_none_or(|ledger| ledger.item != item)
        {
            return Err("stale or incorrect directory Move Replace rename item".into());
        }
        Ok(index)
    }

    fn transition_directory_move_replace_rename(
        &mut self,
        index: usize,
        transition: impl FnOnce(&mut JournalAttempt),
    ) -> Result<(), String> {
        let previous = self.document.clone();
        transition(&mut self.document.attempts[index]);
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn register_directory_move(
        &mut self,
        key: &str,
        ledger: JournalDirectoryMove,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale directory move attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        if attempt.kind != JournalKind::Move
            || !matches!(
                attempt.state,
                JournalState::Preparing | JournalState::Running
            )
            || attempt
                .directory_moves
                .iter()
                .any(|existing| existing.item == ledger.item)
            || ledger.item as usize >= attempt.sources.len()
            || attempt.sources[ledger.item as usize] != ledger.source
        {
            return Err("invalid directory move registration".into());
        }
        validate_directory_children(&ledger)?;
        if ledger.pending.is_some() {
            return Err("registered directory move cannot start with a pending intent".into());
        }
        let root = ledger
            .root
            .as_ref()
            .ok_or_else(|| "directory move registration requires root identities".to_string())?;
        if root.source_identity.is_empty()
            || root.destination_identity.is_empty()
            || root.pending.is_some()
            || root.disposition_recorded
        {
            return Err(
                "registered directory move requires exact untouched root identities".into(),
            );
        }
        if ledger.cleanup_complete
            || ledger.children.iter().any(|child| {
                child.disposition_recorded || child.node_kind == JournalDirectoryNodeKind::Unknown
            })
        {
            return Err(
                "registered directory move must start with an untouched child ledger".into(),
            );
        }
        let mut size_probe = attempt.clone();
        size_probe.directory_moves.push(ledger.clone());
        size_probe
            .directory_moves
            .sort_by_key(|move_record| move_record.item);
        validate_directory_move_vector(&size_probe)?;
        let previous = self.document.clone();
        let ledgers = &mut self.document.attempts[index].directory_moves;
        ledgers.push(ledger);
        ledgers.sort_by_key(|move_record| move_record.item);
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn register_copy_replace_cleanup(
        &mut self,
        key: &str,
        ledger: JournalCopyReplaceCleanup,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale Copy Replace cleanup attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        let backup = attempt
            .replace_backup
            .as_ref()
            .ok_or_else(|| "Copy Replace cleanup requires a backup intent".to_string())?;
        if attempt.kind != JournalKind::Copy
            || !matches!(
                attempt.state,
                JournalState::Preparing | JournalState::Running
            )
            || attempt
                .copy_replace_cleanups
                .iter()
                .any(|existing| existing.item == ledger.item && !existing.cleanup_complete)
            || ledger.item as usize >= attempt.sources.len()
            || backup.destination != ledger.destination
            || backup.backup != ledger.backup
            || ledger.root_identity.is_empty()
            || ledger.pending.is_some()
            || ledger.cleanup_complete
            || ledger.children.iter().enumerate().any(|(index, child)| {
                child.ordinal as usize != index
                    || child.identity.is_empty()
                    || child.content_fact.is_empty()
                    || child.disposition_recorded
                    || child.node_kind == JournalDirectoryNodeKind::Unknown
            })
            || ledger
                .children
                .windows(2)
                .any(|pair| pair[0].relative_components.len() < pair[1].relative_components.len())
        {
            return Err("invalid Copy Replace cleanup manifest".into());
        }
        let mut probe = self.document.attempts[index].clone();
        probe.copy_replace_cleanups.push(ledger.clone());
        if serde_json::to_vec(&probe)
            .map_err(|error| error.to_string())?
            .len()
            > 3 * 1024 * 1024
        {
            return Err("Copy Replace cleanup manifest exceeds journal bound".into());
        }
        let previous = self.document.clone();
        self.document.attempts[index]
            .copy_replace_cleanups
            .push(ledger);
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn record_copy_replace_cleanup_intent(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
    ) -> Result<(), String> {
        self.transition_copy_replace_cleanup(key, item, intent, false)
    }

    fn record_copy_replace_cleanup_receipt(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
    ) -> Result<(), String> {
        self.transition_copy_replace_cleanup(key, item, intent, true)
    }

    fn transition_copy_replace_cleanup(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalCopyReplaceChildIntent,
        receipt: bool,
    ) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale Copy Replace cleanup attempt".to_string())?;
        let ledger_index = self.document.attempts[index]
            .copy_replace_cleanups
            .iter()
            .position(|ledger| ledger.item == item && !ledger.cleanup_complete)
            .ok_or_else(|| "stale Copy Replace cleanup item".to_string())?;
        let previous = self.document.clone();
        let ledger = &mut self.document.attempts[index].copy_replace_cleanups[ledger_index];
        let root_intent = intent.ordinal as usize == ledger.children.len()
            && intent.node_kind == JournalDirectoryNodeKind::Directory
            && intent.relative_components.is_empty()
            && intent.identity == ledger.root_identity
            && intent.content_fact == "root";
        if receipt {
            if ledger.pending.as_ref() != Some(&intent) {
                return Err("stale Copy Replace cleanup receipt".into());
            }
            if root_intent {
                if ledger.root_disposition_recorded {
                    return Err("duplicate Copy Replace root receipt".into());
                }
                ledger.root_disposition_recorded = true;
            } else {
                let child = ledger
                    .children
                    .get_mut(intent.ordinal as usize)
                    .ok_or_else(|| "invalid Copy Replace cleanup ordinal".to_string())?;
                if child.disposition_recorded
                    || child.node_kind != intent.node_kind
                    || child.relative_components != intent.relative_components
                    || child.identity != intent.identity
                    || child.content_fact != intent.content_fact
                {
                    return Err("Copy Replace cleanup receipt does not match manifest".into());
                }
                child.disposition_recorded = true;
            }
            ledger.pending = None;
        } else {
            if ledger.pending.is_some()
                || ledger.root_disposition_recorded
                || ledger.cleanup_complete
            {
                return Err("Copy Replace cleanup already has an active disposition".into());
            }
            let next = ledger
                .children
                .iter()
                .position(|child| !child.disposition_recorded);
            if root_intent {
                if next.is_some() || ledger.root_disposition_recorded {
                    return Err("Copy Replace root disposition is not last".into());
                }
            } else {
                let ordinal = next.ok_or_else(|| {
                    "Copy Replace root disposition must follow descendants".to_string()
                })?;
                let child = &ledger.children[ordinal];
                if child.ordinal != intent.ordinal
                    || child.node_kind != intent.node_kind
                    || child.relative_components != intent.relative_components
                    || child.identity != intent.identity
                    || child.content_fact != intent.content_fact
                {
                    return Err("Copy Replace cleanup intent is out of manifest order".into());
                }
            }
            ledger.pending = Some(intent);
        }
        if let Err(error) = validate_copy_replace_cleanup_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn complete_copy_replace_cleanup(&mut self, key: &str, item: u32) -> Result<(), String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale Copy Replace cleanup attempt".to_string())?;
        let previous = self.document.clone();
        let ledger = self.document.attempts[index]
            .copy_replace_cleanups
            .iter_mut()
            .find(|ledger| ledger.item == item)
            .ok_or_else(|| "stale Copy Replace cleanup item".to_string())?;
        if ledger.pending.is_some()
            || !ledger.root_disposition_recorded
            || ledger
                .children
                .iter()
                .any(|child| !child.disposition_recorded)
        {
            return Err("Copy Replace cleanup is not complete".into());
        }
        ledger.cleanup_complete = true;
        if let Err(error) = validate_copy_replace_cleanup_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn record_directory_move_intent(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryChildIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        if self.document.attempts[index].state != JournalState::Running {
            return Err("directory move child intent requires a running attempt".into());
        }
        let ledger = self.document.attempts[index]
            .directory_moves
            .iter()
            .find(|ledger| ledger.item == item)
            .unwrap();
        let child = ledger
            .children
            .iter()
            .find(|child| child.ordinal == intent.ordinal)
            .ok_or_else(|| "unknown directory move child".to_string())?;
        if !directory_move_source_cleanup_allowed(ledger)
            || ledger.pending.is_some()
            || ledger
                .root
                .as_ref()
                .is_none_or(|root| root.pending.is_some())
            || child.disposition_recorded
            || child.node_kind == JournalDirectoryNodeKind::Unknown
            || child.relative_components != intent.relative_components
            || child.node_kind != intent.node_kind
            || child.source_identity != intent.source_identity
            || child.destination_identity != intent.destination_identity
            || child.content_fact != intent.content_fact
        {
            return Err("directory move intent does not match an available exact child".into());
        }
        self.transition_directory_attempt(index, |attempt| {
            directory_move_for_item_mut(attempt, item).unwrap().pending = Some(intent);
        })
    }

    fn record_directory_move_receipt(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryChildIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let ledger = self.document.attempts[index]
            .directory_moves
            .iter()
            .find(|ledger| ledger.item == item)
            .unwrap();
        if !directory_move_source_cleanup_allowed(ledger)
            || ledger.pending.as_ref() != Some(&intent)
        {
            return Err("directory move receipt has no exact pending intent".into());
        }
        if ledger
            .root
            .as_ref()
            .is_none_or(|root| root.pending.is_some())
        {
            return Err("child receipt is blocked after root intent".into());
        }
        if intent.node_kind == JournalDirectoryNodeKind::Unknown
            || ledger
                .children
                .iter()
                .find(|child| child.ordinal == intent.ordinal)
                .is_none_or(|child| child.node_kind == JournalDirectoryNodeKind::Unknown)
        {
            return Err("directory move receipt requires a known child node kind".into());
        }
        let previous = self.document.clone();
        let ledger = directory_move_for_item_mut(&mut self.document.attempts[index], item).unwrap();
        ledger
            .children
            .iter_mut()
            .find(|child| child.ordinal == intent.ordinal)
            .unwrap()
            .disposition_recorded = true;
        ledger.pending = None;
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn record_directory_move_root_intent(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryRootIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let attempt = &self.document.attempts[index];
        let ledger = directory_move_for_item(attempt, item).unwrap();
        let root = ledger
            .root
            .as_ref()
            .ok_or_else(|| "directory move root state is missing".to_string())?;
        if attempt.state != JournalState::Running
            || !directory_move_source_cleanup_allowed(ledger)
            || ledger.pending.is_some()
            || ledger
                .children
                .iter()
                .any(|child| !child.disposition_recorded)
            || root.pending.is_some()
            || root.disposition_recorded
            || root.source_identity != intent.source_identity
            || root.destination_identity != intent.destination_identity
        {
            return Err("directory move root intent does not match a ready exact root".into());
        }
        self.transition_directory_attempt(index, |attempt| {
            directory_move_for_item_mut(attempt, item)
                .unwrap()
                .root
                .as_mut()
                .unwrap()
                .pending = Some(intent);
        })
    }

    fn record_directory_move_root_receipt(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryRootIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let ledger = self.document.attempts[index]
            .directory_moves
            .iter()
            .find(|ledger| ledger.item == item)
            .unwrap();
        if !directory_move_source_cleanup_allowed(ledger) {
            return Err("directory Move Replace source cleanup is not ready".into());
        }
        let root = ledger
            .root
            .as_ref()
            .ok_or_else(|| "directory move root state is missing".to_string())?;
        if root.pending.as_ref() != Some(&intent) || root.disposition_recorded {
            return Err("directory move root receipt has no exact pending intent".into());
        }
        let previous = self.document.clone();
        let root = directory_move_for_item_mut(&mut self.document.attempts[index], item)
            .unwrap()
            .root
            .as_mut()
            .unwrap();
        root.pending = None;
        root.disposition_recorded = true;
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn complete_directory_move(&mut self, key: &str, item: u32) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let attempt = &self.document.attempts[index];
        let ledger = directory_move_for_item(attempt, item).unwrap();
        if attempt.state != JournalState::Running
            || ledger.pending.is_some()
            || ledger.children.iter().any(|child| {
                !child.disposition_recorded || child.node_kind == JournalDirectoryNodeKind::Unknown
            })
            || ledger.cleanup_complete
            || ledger
                .replace
                .as_ref()
                .is_some_and(|replace| replace.phase != DirectoryMoveReplacePhase::SourceCleanup)
            || ledger
                .root
                .as_ref()
                .is_none_or(|root| root.pending.is_some() || !root.disposition_recorded)
        {
            return Err("directory move cleanup is not ready to complete".into());
        }
        let previous = self.document.clone();
        let ledger = directory_move_for_item_mut(&mut self.document.attempts[index], item).unwrap();
        ledger.cleanup_complete = true;
        if let Some(replace) = &mut ledger.replace {
            replace.phase = DirectoryMoveReplacePhase::SourceComplete;
        }
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    fn advance_directory_move_replace(
        &mut self,
        key: &str,
        item: u32,
        phase: DirectoryMoveReplacePhase,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let attempt = &self.document.attempts[index];
        let current = directory_move_for_item(attempt, item)
            .and_then(|ledger| ledger.replace.as_ref())
            .ok_or_else(|| "directory Move Replace record is missing".to_string())?;
        if attempt.state != JournalState::Running
            || (current.phase != phase && !directory_move_replace_phase_next(current.phase, phase))
            || (phase == DirectoryMoveReplacePhase::Complete
                && (!current.backup_cleanup_complete
                    || !directory_move_for_item(attempt, item)
                        .is_some_and(|ledger| ledger.cleanup_complete)))
        {
            return Err("invalid directory Move Replace phase transition".into());
        }
        if current.phase == phase {
            return Ok(());
        }
        self.transition_directory_attempt(index, |attempt| {
            directory_move_for_item_mut(attempt, item)
                .unwrap()
                .replace
                .as_mut()
                .unwrap()
                .phase = phase;
        })
    }

    fn record_directory_move_replace_backup_intent(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryMoveReplaceChildIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let attempt = &self.document.attempts[index];
        let ledger = directory_move_for_item(attempt, item).unwrap();
        let replace = ledger
            .replace
            .as_ref()
            .ok_or_else(|| "directory Move Replace record is missing".to_string())?;
        if attempt.state != JournalState::Running
            || replace.phase != DirectoryMoveReplacePhase::BackupCleanup
            || !ledger.cleanup_complete
            || replace.backup_cleanup_complete
            || replace.backup_pending.is_some()
        {
            return Err("directory Move Replace backup cleanup is not ready".into());
        }
        let root_intent = intent.ordinal as usize == replace.backup_children.len()
            && intent.node_kind == JournalDirectoryNodeKind::Directory
            && intent.relative_components.is_empty()
            && intent.identity == replace.backup_root_identity
            && intent.content_fact == "root"
            && replace
                .backup_children
                .iter()
                .all(|child| child.disposition_recorded);
        if !root_intent {
            let child = replace
                .backup_children
                .get(intent.ordinal as usize)
                .ok_or_else(|| {
                    "directory Move Replace backup intent has invalid ordinal".to_string()
                })?;
            if child.disposition_recorded
                || child.ordinal != intent.ordinal
                || child.node_kind != intent.node_kind
                || child.relative_components != intent.relative_components
                || child.identity != intent.identity
                || child.content_fact != intent.content_fact
            {
                return Err(
                    "directory Move Replace backup intent is not the next manifest item".into(),
                );
            }
        }
        self.transition_directory_attempt(index, |attempt| {
            directory_move_for_item_mut(attempt, item)
                .unwrap()
                .replace
                .as_mut()
                .unwrap()
                .backup_pending = Some(intent);
        })
    }

    fn record_directory_move_replace_backup_receipt(
        &mut self,
        key: &str,
        item: u32,
        intent: JournalDirectoryMoveReplaceChildIntent,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        if self.document.attempts[index].state != JournalState::Running {
            return Err("directory Move Replace backup receipt requires a running attempt".into());
        }
        let replace = directory_move_for_item(&self.document.attempts[index], item)
            .and_then(|ledger| ledger.replace.as_ref())
            .ok_or_else(|| "directory Move Replace record is missing".to_string())?;
        if replace.phase != DirectoryMoveReplacePhase::BackupCleanup
            || replace.backup_pending.as_ref() != Some(&intent)
        {
            return Err("directory Move Replace backup receipt has no exact intent".into());
        }
        self.transition_directory_attempt(index, |attempt| {
            let replace = directory_move_for_item_mut(attempt, item)
                .unwrap()
                .replace
                .as_mut()
                .unwrap();
            if intent.ordinal as usize == replace.backup_children.len() {
                replace.backup_root_disposition_recorded = true;
            } else {
                replace.backup_children[intent.ordinal as usize].disposition_recorded = true;
            }
            replace.backup_pending = None;
        })
    }

    fn complete_directory_move_replace_backup(
        &mut self,
        key: &str,
        item: u32,
    ) -> Result<(), String> {
        let index = self.directory_move_index(key, item)?;
        let attempt = &self.document.attempts[index];
        let ledger = directory_move_for_item(attempt, item).unwrap();
        let replace = ledger
            .replace
            .as_ref()
            .ok_or_else(|| "directory Move Replace record is missing".to_string())?;
        if attempt.state != JournalState::Running
            || !ledger.cleanup_complete
            || replace.phase != DirectoryMoveReplacePhase::BackupCleanup
            || replace.backup_pending.is_some()
            || replace.backup_cleanup_complete
            || !replace.backup_root_disposition_recorded
            || replace
                .backup_children
                .iter()
                .any(|child| !child.disposition_recorded)
        {
            return Err("directory Move Replace backup cleanup is not complete".into());
        }
        self.transition_directory_attempt(index, |attempt| {
            let replace = directory_move_for_item_mut(attempt, item)
                .unwrap()
                .replace
                .as_mut()
                .unwrap();
            replace.backup_cleanup_complete = true;
            replace.phase = DirectoryMoveReplacePhase::BackupComplete;
        })
    }

    fn directory_move_index(&self, key: &str, item: u32) -> Result<usize, String> {
        let index = self
            .document
            .attempts
            .iter()
            .position(|attempt| attempt.key == key)
            .ok_or_else(|| "stale directory move attempt".to_string())?;
        let attempt = &self.document.attempts[index];
        if attempt.kind != JournalKind::Move
            || attempt
                .directory_moves
                .iter()
                .all(|ledger| ledger.item != item)
        {
            return Err("stale or incorrect directory move item".into());
        }
        Ok(index)
    }

    fn transition_directory_attempt(
        &mut self,
        index: usize,
        transition: impl FnOnce(&mut JournalAttempt),
    ) -> Result<(), String> {
        let previous = self.document.clone();
        transition(&mut self.document.attempts[index]);
        if let Err(error) = validate_directory_move_vector(&self.document.attempts[index]) {
            self.document = previous;
            return Err(error);
        }
        if let Err(error) = self.flush() {
            self.document = previous;
            return Err(error);
        }
        Ok(())
    }

    /// User-approved discard removes only the specific recovery item.  It does
    /// not touch the filesystem object described by that item.
    pub fn discard(&mut self, key: &str) -> Result<bool, String> {
        let previous = self.document.clone();
        let before = self.document.attempts.len();
        self.document.attempts.retain(|attempt| attempt.key != key);
        if before == self.document.attempts.len() {
            return Ok(false);
        }
        match self.flush() {
            Ok(()) => Ok(true),
            Err(error) => {
                self.document = previous;
                Err(error)
            }
        }
    }

    fn flush(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec(&self.document).map_err(|error| error.to_string())?;
        let reserved_growth = self
            .document
            .attempts
            .iter()
            .try_fold(0usize, |sum, attempt| -> Result<usize, String> {
                let growth = seven_zip_future_growth(attempt)?;
                sum.checked_add(growth).ok_or_else(|| "operation journal reservation overflow".into())
            })?;
        if bytes.len().checked_add(reserved_growth).is_none_or(|size| size > MAX_BYTES) {
            return Err("operation journal exceeds size limit".into());
        }
        self.file.replace(&bytes).map_err(|error| error.to_string())
    }
}

fn directory_move_for_item(attempt: &JournalAttempt, item: u32) -> Option<&JournalDirectoryMove> {
    attempt
        .directory_moves
        .iter()
        .find(|ledger| ledger.item == item)
}

fn directory_move_for_item_mut(
    attempt: &mut JournalAttempt,
    item: u32,
) -> Option<&mut JournalDirectoryMove> {
    attempt
        .directory_moves
        .iter_mut()
        .find(|ledger| ledger.item == item)
}

fn directory_move_source_cleanup_allowed(ledger: &JournalDirectoryMove) -> bool {
    ledger.replace.as_ref().map_or(true, |replace| {
        replace.phase == DirectoryMoveReplacePhase::SourceCleanup
    })
}

pub(crate) fn directory_move_replace_phase_next(
    from: DirectoryMoveReplacePhase,
    to: DirectoryMoveReplacePhase,
) -> bool {
    use DirectoryMoveReplacePhase::*;
    matches!(
        (from, to),
        (Staged, DisplaceIntent)
            | (Staged, StageDiscarded)
            | (DisplaceIntent, StageDiscarded)
            | (DisplaceIntent, TargetDisplaced)
            | (TargetDisplaced, PublishIntent)
            | (PublishIntent, DestinationPublished)
            | (DestinationPublished, SourceCleanup)
            | (SourceComplete, BackupCleanup)
            | (BackupComplete, Complete)
    )
}

pub(crate) fn cross_volume_phase_next(
    from: CrossVolumeMoveReplacePhase,
    to: CrossVolumeMoveReplacePhase,
) -> bool {
    use CrossVolumeMoveReplacePhase::*;
    matches!(
        (from, to),
        (StageCreateIntent, StageOwned)
            | (StageCreateIntent, StageNotCreated)
            | (StageCreateIntent, StageDiscarded)
            | (StageOwned, StageReady)
            | (StageOwned, StageDiscarded)
            | (StageReady, DisplaceIntent)
            | (StageReady, StageDiscarded)
            | (DisplaceIntent, TargetDisplaced)
            | (DisplaceIntent, StageDiscarded)
            | (TargetDisplaced, PublishIntent)
            | (PublishIntent, DestinationPublished)
            | (DestinationPublished, SourceDeleteIntent)
            | (SourceDeleteIntent, SourceDeleted)
            | (SourceDeleted, BackupDeleteIntent)
            | (BackupDeleteIntent, BackupDeleted)
            | (BackupDeleted, Complete)
    )
}

pub(crate) fn cross_volume_move_replace_same_identity(
    a: &JournalCrossVolumeMoveReplace,
    b: &JournalCrossVolumeMoveReplace,
) -> bool {
    a.item == b.item
        && a.source == b.source
        && a.stage == b.stage
        && a.destination == b.destination
        && a.backup == b.backup
        && a.source_identity == b.source_identity
        && a.approved_target_identity == b.approved_target_identity
        && a.destination_parent_identity == b.destination_parent_identity
}

fn validate_cross_volume_move_replace(attempt: &JournalAttempt) -> Result<(), String> {
    let Some(ledger) = &attempt.cross_volume_move_replace else {
        return Ok(());
    };
    if attempt.kind != JournalKind::Move
        || attempt.sources.len() != 1
        || ledger.item != 0
        || attempt.sources.first() != Some(&ledger.source)
        || ledger.source == ledger.stage
        || ledger.stage == ledger.destination
        || ledger.destination == ledger.backup
        || ledger.stage == ledger.backup
        || ledger.source_identity.is_empty()
        || ledger.approved_target_identity.is_empty()
        || ledger.destination_parent_identity.is_empty()
        || match ledger.phase {
            CrossVolumeMoveReplacePhase::StageCreateIntent | CrossVolumeMoveReplacePhase::StageNotCreated => !ledger.stage_identity.is_empty() || !ledger.content_sha256.is_empty(),
            CrossVolumeMoveReplacePhase::StageOwned => ledger.stage_identity.is_empty() || !ledger.content_sha256.is_empty(),
            CrossVolumeMoveReplacePhase::StageDiscarded => ledger.stage_identity.is_empty() || (!ledger.content_sha256.is_empty() && (ledger.content_sha256.len() != 64 || !ledger.content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()))),
            _ => ledger.stage_identity.is_empty() || ledger.content_sha256.len() != 64 || !ledger.content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
        }
        || attempt.replace_backup.as_ref().is_none_or(|backup| {
            backup.source != ledger.source
                || backup.destination != ledger.destination
                || backup.backup != ledger.backup
        })
    {
        return Err("invalid cross-volume Move Replace ledger".into());
    }
    let identity_volume = |identity: &str| -> Option<String> {
        let (volume, file_index) = identity.split_once(':')?;
        (volume.len() == 8
            && file_index.len() == 16
            && volume.bytes().all(|byte| byte.is_ascii_hexdigit())
            && file_index.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| volume.to_owned())
    };
    let source_volume = identity_volume(&ledger.source_identity)
        .ok_or_else(|| "invalid source identity encoding".to_string())?;
    let target_volume = identity_volume(&ledger.approved_target_identity)
        .ok_or_else(|| "invalid target identity encoding".to_string())?;
    let parent_volume = identity_volume(&ledger.destination_parent_identity)
        .ok_or_else(|| "invalid destination parent identity encoding".to_string())?;
    if source_volume == target_volume
        || parent_volume != target_volume
        || ledger.source_identity == ledger.approved_target_identity
    {
        return Err(
            "cross-volume Move Replace identities do not prove distinct volumes and objects".into(),
        );
    }
    if !ledger.stage_identity.is_empty() {
        let stage_identity = &ledger.stage_identity;
        let stage_volume = identity_volume(stage_identity).ok_or_else(|| "invalid stage identity encoding".to_string())?;
        if stage_volume != target_volume || ledger.source_identity == *stage_identity || *stage_identity == ledger.approved_target_identity {
            return Err("cross-volume Move Replace stage identity is inconsistent".into());
        }
    }
    if ledger.phase == CrossVolumeMoveReplacePhase::Complete
        && matches!(
            attempt.state,
            JournalState::RecoveryRequired | JournalState::CancelRequested
        )
    {
        return Err(
            "completed cross-volume Move Replace cannot be recovery or cancel requested".into(),
        );
    }
    if attempt.state == JournalState::Terminal {
        let terminal_matches = match ledger.phase {
            CrossVolumeMoveReplacePhase::Complete => attempt.terminal_outcome == Some(JournalTerminalOutcome::Succeeded),
            CrossVolumeMoveReplacePhase::StageNotCreated | CrossVolumeMoveReplacePhase::StageDiscarded => matches!(attempt.terminal_outcome, Some(JournalTerminalOutcome::Failed | JournalTerminalOutcome::Cancelled)),
            _ => true,
        };
        if !terminal_matches {
            return Err("cross-volume Move Replace terminal outcome does not match its durable phase".into());
        }
    }
    Ok(())
}

fn validate_zip_create(attempt: &JournalAttempt) -> Result<(), String> {
    let Some(ledger) = &attempt.zip_create else {
        return Ok(());
    };
    let ledger_sources = ledger
        .sources
        .iter()
        .map(|source| source.source.clone())
        .collect::<Vec<_>>();
    if attempt.kind != JournalKind::CreateZip
        || ledger.sources.is_empty()
        || ledger.sources.len() > 1024
        || attempt.sources != ledger_sources
        || attempt.destination.as_ref() != Some(&ledger.output)
        || ledger
            .sources
            .iter()
            .any(|source| source.source.units.is_empty() || source.source_identity.is_empty())
        || ledger_sources
            .iter()
            .enumerate()
            .any(|(index, source)| ledger_sources[..index].contains(source))
        || ledger.output.units.is_empty()
        || ledger.parent_identity.is_empty()
        || ledger.stage.units.is_empty()
        || (ledger.phase != JournalZipCreatePhase::StagePlanned
            && ledger.phase != JournalZipCreatePhase::StageDiscarded
            && ledger.stage_identity.as_deref().is_none_or(str::is_empty))
        || ledger.output_identity.as_deref().is_some_and(str::is_empty)
        || (ledger.phase != JournalZipCreatePhase::Published && ledger.output_identity.is_some())
    {
        return Err("invalid ZIP create journal ledger".into());
    }
    Ok(())
}

fn validate_seven_zip_create(attempt: &JournalAttempt) -> Result<(), String> {
    let Some(ledger) = &attempt.seven_zip_create else {
        return Ok(());
    };
    if attempt.kind != JournalKind::Create7z
        || attempt.zip_create.is_some()
        || attempt.zip_extract.is_some()
        || !attempt.zip_extract_items.is_empty()
        || !attempt.zip_extract_receipts.is_empty()
        || attempt.created_folder_path.is_some()
        || attempt.replace_backup.is_some()
        || attempt.cross_volume_move_replace.is_some()
        || attempt.rename_stage.is_some()
        || !attempt.directory_moves.is_empty()
        || !attempt.copy_replace_cleanups.is_empty()
        || !attempt.batch_rename_plan.is_empty()
        || !attempt.batch_rename_completed.is_empty()
        || attempt.virtual_source_name.is_some()
        || attempt.virtual_source_size.is_some()
        || !attempt.virtual_sources.is_empty()
        || !attempt.virtual_item_status.is_empty()
        || attempt.sources
            != ledger
                .sources
                .iter()
                .map(|source| source.source.clone())
                .collect::<Vec<_>>()
        || attempt.destination.as_ref() != Some(&ledger.output)
    {
        return Err("7z ledger does not match its operation record".into());
    }
    ledger.validate()?;
    if attempt.state == JournalState::Terminal {
        match (ledger.phase, attempt.terminal_outcome) {
            (JournalSevenZipCreatePhase::Published, Some(JournalTerminalOutcome::Succeeded))
                if attempt.receipts_recorded
                    && attempt.created_archive_path.as_ref() == Some(&ledger.output) => {}
            (
                JournalSevenZipCreatePhase::Discarded,
                Some(JournalTerminalOutcome::Cancelled | JournalTerminalOutcome::Failed),
            ) if attempt.receipts_recorded => {}
            (_, Some(JournalTerminalOutcome::Partial | JournalTerminalOutcome::Unknown)) => {}
            _ => return Err("7z terminal snapshot overstates its durable ledger phase".into()),
        }
    }
    Ok(())
}

fn validate_seven_zip_create_handles(attempt: &JournalAttempt) -> Result<(), String> {
    let Some(ledger) = &attempt.seven_zip_create_handles else {
        return Ok(());
    };
    if attempt.kind != JournalKind::Create7z
        || attempt.seven_zip_create.is_some()
        || attempt.zip_create.is_some()
        || attempt.zip_extract.is_some()
        || !attempt.zip_extract_items.is_empty()
        || !attempt.zip_extract_receipts.is_empty()
        || attempt.created_folder_path.is_some()
        || attempt.replace_backup.is_some()
        || attempt.cross_volume_move_replace.is_some()
        || attempt.rename_stage.is_some()
        || !attempt.directory_moves.is_empty()
        || !attempt.copy_replace_cleanups.is_empty()
        || !attempt.batch_rename_plan.is_empty()
        || !attempt.batch_rename_completed.is_empty()
        || attempt.virtual_source_name.is_some()
        || attempt.virtual_source_size.is_some()
        || !attempt.virtual_sources.is_empty()
        || !attempt.virtual_item_status.is_empty()
        || attempt.sources
            != ledger
                .sources
                .iter()
                .map(|source| source.path.clone())
                .collect::<Vec<_>>()
        || attempt.destination.as_ref() != Some(&ledger.output)
        || attempt.created_archive_path.as_ref().is_some_and(|path| {
            path != &ledger.output || ledger.phase != JournalSevenZipHandlePhase::Published
        })
    {
        return Err("handle-based 7z ledger does not match its operation record".into());
    }
    ledger.validate()?;
    if seven_zip_handle_future_size(attempt)? > MAX_DIRECTORY_ATTEMPT_BYTES {
        return Err(
            "handle-based 7z attempt cannot reserve its complete bounded future ledger growth"
                .into(),
        );
    }
    if attempt.state == JournalState::Terminal {
        match (ledger.phase, attempt.terminal_outcome) {
            (JournalSevenZipHandlePhase::Published, Some(JournalTerminalOutcome::Succeeded))
                if attempt.receipts_recorded
                    && attempt.created_archive_path.as_ref() == Some(&ledger.output) => {}
            (
                JournalSevenZipHandlePhase::Discarded,
                Some(JournalTerminalOutcome::Failed | JournalTerminalOutcome::Cancelled),
            ) if attempt.receipts_recorded => {}
            (_, Some(JournalTerminalOutcome::Partial | JournalTerminalOutcome::Unknown)) => {}
            _ => {
                return Err(
                    "terminal Create7z attempt lacks a matching handle-ledger receipt".into(),
                );
            }
        }
    }
    Ok(())
}

fn seven_zip_future_growth(attempt: &JournalAttempt) -> Result<usize, String> {
    let current = serde_json::to_vec(attempt)
        .map_err(|error| error.to_string())?
        .len();
    let handles = if attempt.seven_zip_create_handles.is_some() {
        seven_zip_handle_future_size(attempt)?.saturating_sub(current)
    } else {
        0
    };
    let extract = if attempt.seven_zip_extract_batch.is_some() {
        seven_zip_extract_batch_future_size(attempt)?.checked_sub(current)
            .ok_or("7z extraction future projection is smaller than current attempt")?
    } else { 0 };
    handles.checked_add(extract).ok_or_else(|| "7z future reservation overflow".into())
}

fn seven_zip_extract_batch_future_size(attempt: &JournalAttempt) -> Result<usize, String> {
    use JournalSevenZipExtractObjectState as State;
    use JournalSevenZipExtractPhase as Phase;
    let batch = attempt.seven_zip_extract_batch.as_ref().ok_or("missing 7z extraction batch")?;
    let mut projection = attempt.clone();
    projection.key = format!("r:{}:{}:{}", hex_run_id(&batch.envelope.attempt.run_id), u64::MAX, u64::MAX);
    let projected = projection.seven_zip_extract_batch.as_mut().expect("cloned batch");
    projected.sequence = u64::MAX;
    projected.envelope.attempt.job = u64::MAX;
    projected.envelope.attempt.attempt = u64::MAX;
    projected.envelope.provider_revision = u64::MAX;
    projected.envelope.destination_parent = JournalSevenZipExtractIdentity { volume: u32::MAX, file_index: u64::MAX };
    for binding in [&mut projected.envelope.provider_executable, &mut projected.envelope.provider_dll, &mut projected.envelope.application_image] {
        binding.identity = JournalSevenZipExtractIdentity { volume: u32::MAX, file_index: u64::MAX };
    }
    for descriptor in &mut projected.envelope.items {
        descriptor.source_identity = JournalSevenZipExtractIdentity { volume: u32::MAX, file_index: u64::MAX };
    }
    projected.items.clear();
    for descriptor in &batch.envelope.items {
        let identity = JournalSevenZipExtractIdentity { volume: u32::MAX, file_index: u64::MAX };
        let binding = JournalSevenZipExtractBinding {
            attempt: batch.envelope.attempt.clone(),
            item: descriptor.ordinal,
            source: JournalSevenZipExtractSource {
                path: descriptor.source_path.clone(), identity: descriptor.source_identity.clone(),
                length: u64::MAX, last_write_ticks: u64::MAX,
            },
            provider_revision: batch.envelope.provider_revision,
            provider_executable: batch.envelope.provider_executable.clone(),
            provider_dll: batch.envelope.provider_dll.clone(),
            application_image: batch.envelope.application_image.clone(),
            destination_parent: batch.envelope.destination_parent.clone(),
            output: descriptor.output.clone(), spool: descriptor.spool.clone(), tree: descriptor.tree.clone(),
            plan: JournalSevenZipExtractPlan {
                plan_format: u32::MAX, sha256: [u8::MAX; 32], indexed_record_count: u32::MAX,
                materialized_node_count: u32::MAX, file_count: u32::MAX, total_expanded_bytes: u64::MAX,
            },
        };
        projected.items.push(JournalSevenZipExtractLedger {
            // サイズ上限だけを測るため、最大文字数の表記を選ぶ。これは有効な遷移ではない。
            binding, sequence: u64::MAX, aborting: false, phase: Phase::SpoolCreateIntent,
            spool_state: State::CreateFailed, spool_identity: Some(identity.clone()),
            tree_state: State::DeleteIntent, tree_identity: Some(identity.clone()),
            spool_ready: Some(JournalSevenZipExtractSpoolReady { plan_format: u32::MAX, plan_sha256: [u8::MAX;32], length: u64::MAX }),
            tree_ready: Some(JournalSevenZipExtractTreeReady { plan_format: u32::MAX, plan_sha256: [u8::MAX;32], materialized_node_count: u32::MAX, file_count: u32::MAX, total_verified_bytes: u64::MAX }),
            publication_identity: Some(identity),
        });
    }
    projection.seven_zip_extract_receipts = batch.envelope.items.iter().map(|item| JournalSevenZipExtractTerminalReceipt {
        ordinal: item.ordinal, outcome: JournalSevenZipExtractOutcome::CancelledBeforeStart,
        output: Some(item.output.clone()), publication_identity: Some(JournalSevenZipExtractIdentity { volume: u32::MAX, file_index: u64::MAX }),
    }).collect();
    projection.receipts_recorded = false;
    projection.state = JournalState::RecoveryRequired;
    projection.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
    let bytes = serde_json::to_vec(&projection).map_err(|error| error.to_string())?.len();
    if bytes > MAX_DIRECTORY_ATTEMPT_BYTES {
        return Err("7z extraction batch cannot reserve its complete bounded future ledger growth".into());
    }
    Ok(bytes)
}

fn seven_zip_handle_future_size(attempt: &JournalAttempt) -> Result<usize, String> {
    let Some(ledger) = &attempt.seven_zip_create_handles else {
        return Ok(0);
    };
    let mut projection = attempt.clone();
    let future = projection
        .seven_zip_create_handles
        .as_mut()
        .expect("cloned handle ledger");
    let payload_bytes = ledger
        .manifest
        .iter()
        .filter(|entry| !entry.directory)
        .try_fold(0u64, |sum, entry| sum.checked_add(entry.length))
        .ok_or("7z captured payload size overflow")?;
    let max_length = payload_bytes
        .checked_mul(2)
        .and_then(|value| value.checked_add(8 * 1024 * 1024))
        .and_then(|value| value.checked_add(32))
        .ok_or("7z archive size bound overflow")?;
    future.sequence = u64::MAX;
    future.aborting = false;
    let max_identity = JournalSevenZipHandleIdentity {
        volume: u32::MAX,
        file_index: u64::MAX,
    };
    future.candidate.state = JournalSevenZipHandleObjectState::Deleted;
    future.candidate.identity = Some(max_identity.clone());
    future.candidate.length = Some(max_length);
    future.stage.state = JournalSevenZipHandleObjectState::Published;
    future.stage.identity = Some(max_identity.clone());
    future.stage.length = Some(max_length);
    future.phase = JournalSevenZipHandlePhase::Published;
    future.archive_identity = Some(max_identity);
    projection.created_archive_path = Some(ledger.output.clone());
    projection.receipts_recorded = true;
    projection.state = JournalState::RecoveryRequired;
    projection.terminal_outcome = Some(JournalTerminalOutcome::Partial);
    serde_json::to_vec(&projection)
        .map(|bytes| bytes.len())
        .map_err(|error| error.to_string())
}

fn validate_zip_extract(attempt: &JournalAttempt) -> Result<(), String> {
    if attempt.zip_extract.is_some() && !attempt.zip_extract_items.is_empty() {
        return Err("singleton and batch ZIP extract ledgers cannot be combined".into());
    }
    if attempt
        .zip_extract_items
        .windows(2)
        .any(|pair| pair[0].item >= pair[1].item)
    {
        return Err("ZIP extract item ledgers must be unique and ordered".into());
    }
    if attempt
        .zip_extract_receipts
        .windows(2)
        .any(|pair| pair[0].item >= pair[1].item)
        || attempt
            .zip_extract_receipts
            .iter()
            .any(|receipt| receipt.item as usize >= attempt.sources.len())
    {
        return Err("ZIP extract receipts must be unique, ordered, and source-bound".into());
    }
    for entry in &attempt.zip_extract_items {
        let ledger = &entry.ledger;
        let expected_output = attempt
            .sources
            .get(entry.item as usize)
            .and_then(JournalPath::to_path)
            .and_then(|source| Some(source.parent()?.join(source.file_stem()?)))
            .map(|path| JournalPath::from_path(&path));
        if attempt.kind != JournalKind::ExtractZip
            || entry.item as usize >= attempt.sources.len()
            || attempt.sources[entry.item as usize] != ledger.source
            || expected_output.as_ref() != Some(&ledger.output)
            || ledger.source_identity.is_empty()
            || ledger.parent_identity.is_empty()
            || ledger.stage.units.is_empty()
            || (ledger.phase != JournalZipExtractPhase::StagePlanned
                && ledger.phase != JournalZipExtractPhase::StageDiscarded
                && ledger.stage_identity.as_deref().is_none_or(str::is_empty))
        {
            return Err("invalid ZIP extract item ledger".into());
        }
    }
    if !attempt.zip_extract_items.is_empty() {
        if attempt.state == JournalState::Terminal {
            if !attempt.receipts_recorded
                || attempt.zip_extract_receipts.len() != attempt.sources.len()
            {
                return Err(
                    "terminal ZIP extract attempt needs one durable receipt per source".into(),
                );
            }
            for receipt in &attempt.zip_extract_receipts {
                let ledger = attempt
                    .zip_extract_items
                    .iter()
                    .find(|entry| entry.item == receipt.item)
                    .map(|entry| &entry.ledger);
                let valid = match (ledger, receipt.outcome) {
                    (
                        None,
                        JournalZipExtractItemOutcome::Failed
                        | JournalZipExtractItemOutcome::CancelledBeforeStart,
                    ) => receipt.output.is_none(),
                    (Some(ledger), JournalZipExtractItemOutcome::Completed) => {
                        ledger.phase == JournalZipExtractPhase::Published
                            && receipt.output.as_ref() == Some(&ledger.output)
                    }
                    (
                        Some(ledger),
                        JournalZipExtractItemOutcome::Failed
                        | JournalZipExtractItemOutcome::CancelledBeforeStart,
                    ) => {
                        ledger.phase == JournalZipExtractPhase::StageDiscarded
                            && receipt.output.is_none()
                    }
                    (
                        Some(ledger),
                        JournalZipExtractItemOutcome::Partial
                        | JournalZipExtractItemOutcome::Unknown,
                    ) => {
                        matches!(
                            ledger.phase,
                            JournalZipExtractPhase::StageOwned
                                | JournalZipExtractPhase::PublishIntent
                                | JournalZipExtractPhase::Published
                        ) && match ledger.phase {
                            JournalZipExtractPhase::StageOwned => {
                                ledger
                                    .stage_identity
                                    .as_ref()
                                    .is_some_and(|id| !id.is_empty())
                                    && receipt.output.as_ref() == Some(&ledger.stage)
                            }
                            JournalZipExtractPhase::PublishIntent => {
                                (ledger
                                    .stage_identity
                                    .as_ref()
                                    .is_some_and(|id| !id.is_empty())
                                    && receipt.output.as_ref() == Some(&ledger.stage))
                                    || receipt.output.as_ref() == Some(&ledger.output)
                            }
                            JournalZipExtractPhase::Published => {
                                receipt.output.as_ref() == Some(&ledger.output)
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if !valid {
                    return Err("ZIP extract receipt does not resolve its exact item ledger".into());
                }
            }
        } else if !attempt.zip_extract_receipts.is_empty() {
            return Err("ZIP extract receipts cannot precede terminal commit".into());
        }
    }
    let Some(ledger) = &attempt.zip_extract else {
        return Ok(());
    };
    if attempt.zip_create.is_some()
        || attempt.kind != JournalKind::ExtractZip
        || attempt.sources.len() != 1
        || attempt.sources[0] != ledger.source
        || attempt.destination.as_ref() != Some(&ledger.output)
        || ledger.source_identity.is_empty()
        || ledger.parent_identity.is_empty()
        || ledger.stage.units.is_empty()
        || (ledger.phase != JournalZipExtractPhase::StagePlanned
            && ledger.phase != JournalZipExtractPhase::StageDiscarded
            && ledger.stage_identity.as_deref().is_none_or(str::is_empty))
    {
        return Err("invalid ZIP extract journal ledger".into());
    }
    Ok(())
}

fn validate_seven_zip_extract_batch(attempt: &JournalAttempt) -> Result<(), String> {
    let has_facts = attempt.seven_zip_extract_batch.is_some() || !attempt.seven_zip_extract_receipts.is_empty();
    if !has_facts && attempt.kind != JournalKind::Extract7z { return Ok(()); }
    if attempt.kind != JournalKind::Extract7z
        || attempt.zip_create.is_some() || attempt.seven_zip_create.is_some()
        || attempt.seven_zip_create_handles.is_some() || attempt.zip_extract.is_some()
        || !attempt.zip_extract_items.is_empty() || !attempt.zip_extract_receipts.is_empty()
        || attempt.created_folder_path.is_some() || attempt.created_archive_path.is_some()
        || attempt.replace_backup.is_some() || attempt.cross_volume_move_replace.is_some()
        || attempt.rename_stage.is_some() || !attempt.directory_moves.is_empty()
        || !attempt.copy_replace_cleanups.is_empty() || !attempt.batch_rename_plan.is_empty()
        || !attempt.batch_rename_completed.is_empty() || attempt.virtual_source_name.is_some()
        || attempt.virtual_source_size.is_some() || !attempt.virtual_sources.is_empty()
        || !attempt.virtual_item_status.is_empty()
    {
        return Err("7z extraction facts cannot be mixed with another operation ledger".into());
    }
    if let Some(batch) = &attempt.seven_zip_extract_batch {
        batch.validate()?;
        if attempt.sources != batch.envelope.items.iter().map(|item| item.source_path.clone()).collect::<Vec<_>>()
            || attempt.destination.as_ref() != batch.envelope.items.first().map(|item| &item.output)
        {
            return Err("7z extraction envelope does not match ordered request paths".into());
        }
        let key = attempt.key.split(':').collect::<Vec<_>>();
        if key.len() != 4 || key[0] != "r"
            || key[1] != hex_run_id(&batch.envelope.attempt.run_id)
            || key[2].parse::<u64>().ok() != Some(batch.envelope.attempt.job)
            || key[3].parse::<u64>().ok() != Some(batch.envelope.attempt.attempt)
        {
            return Err("7z extraction attempt identity differs from its journal key".into());
        }
    }
    if !attempt.seven_zip_extract_receipts.is_empty() || attempt.terminal_outcome.is_some()
        || attempt.state == JournalState::Terminal
    {
        if attempt.terminal_outcome.is_none() {
            return Err("7z extraction terminal receipts require a terminal aggregate outcome".into());
        }
        if !attempt.receipts_recorded || attempt.seven_zip_extract_receipts.len() != attempt.sources.len() {
            return Err("7z extraction terminal result needs one immutable receipt per source".into());
        }
        validate_seven_zip_extract_receipts(
            &attempt.seven_zip_extract_receipts,
            attempt.sources.len(),
            attempt.seven_zip_extract_batch.as_ref().map(|batch| batch.items.as_slice()).unwrap_or(&[]),
        )?;
        {
            let outcomes = &attempt.seven_zip_extract_receipts;
            let expected = if outcomes.iter().all(|r| r.outcome == JournalSevenZipExtractOutcome::Completed) {
                Some(JournalTerminalOutcome::Succeeded)
            } else if outcomes.iter().all(|r| r.outcome == JournalSevenZipExtractOutcome::Failed) {
                Some(JournalTerminalOutcome::Failed)
            } else if outcomes.iter().all(|r| r.outcome == JournalSevenZipExtractOutcome::CancelledBeforeStart) {
                Some(JournalTerminalOutcome::Cancelled)
            } else if outcomes.iter().any(|r| matches!(r.outcome, JournalSevenZipExtractOutcome::Partial | JournalSevenZipExtractOutcome::Unknown))
                || outcomes.iter().any(|r| r.outcome == JournalSevenZipExtractOutcome::Completed)
            {
                if outcomes.iter().any(|r| r.outcome == JournalSevenZipExtractOutcome::Unknown) {
                    Some(JournalTerminalOutcome::Unknown)
                } else { Some(JournalTerminalOutcome::Partial) }
            } else { Some(JournalTerminalOutcome::Partial) };
            if attempt.terminal_outcome != expected {
                return Err("7z extraction aggregate terminal outcome contradicts its receipts".into());
            }
        }
    }
    Ok(())
}

fn validate_directory_move_vector(attempt: &JournalAttempt) -> Result<(), String> {
    validate_directory_move_replace_rename(attempt)?;
    if attempt
        .directory_moves
        .windows(2)
        .any(|pair| pair[0].item >= pair[1].item)
    {
        return Err("directory move items must be unique and sorted".into());
    }
    for ledger in &attempt.directory_moves {
        if ledger.item as usize >= attempt.sources.len()
            || attempt.sources[ledger.item as usize] != ledger.source
        {
            return Err("directory move item does not match its source path".into());
        }
        validate_directory_children(ledger)?;
        if let Some(replace) = &ledger.replace {
            validate_directory_move_replace(attempt, ledger, replace)?;
        }
    }
    if !attempt.directory_moves.is_empty()
        && serde_json::to_vec(attempt)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_DIRECTORY_ATTEMPT_BYTES
    {
        return Err("directory move attempt exceeds its serialized size limit".into());
    }
    Ok(())
}

pub(crate) fn directory_move_replace_rename_phase_next(
    from: DirectoryMoveReplaceRenamePhase,
    to: DirectoryMoveReplaceRenamePhase,
) -> bool {
    use DirectoryMoveReplaceRenamePhase::*;
    matches!(
        (from, to),
        (Prepared, NoEffect)
            | (Prepared, DisplaceIntent)
            | (DisplaceIntent, NoEffect)
            | (DisplaceIntent, TargetDisplaced)
            | (TargetDisplaced, PublishIntent)
            | (PublishIntent, DestinationPublished)
            | (DestinationPublished, BackupCleanup)
            | (BackupCleanup, BackupComplete)
            | (BackupComplete, Complete)
    )
}

fn directory_move_replace_rename_same_identity(
    old: &JournalDirectoryMoveReplaceRename,
    new: &JournalDirectoryMoveReplaceRename,
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
            old.ordinal == new.ordinal
                && old.node_kind == new.node_kind
                && old.relative_components == new.relative_components
                && old.identity == new.identity
                && old.content_fact == new.content_fact
        })
}

fn validate_directory_move_replace_rename(attempt: &JournalAttempt) -> Result<(), String> {
    use DirectoryMoveReplaceRenamePhase as Phase;
    use DirectoryMoveReplaceSourceLocation as Source;
    use DirectoryMoveReplaceTargetLocation as Target;
    let Some(ledger) = &attempt.directory_move_replace_rename else {
        return Ok(());
    };

    let source = ledger.source.to_path();
    let destination = ledger.destination.to_path();
    let backup = ledger.backup.to_path();
    let source_parent = ledger.source_parent.to_path();
    let destination_parent = ledger.destination_parent.to_path();
    let paths_are_absolute = [&source, &destination, &backup, &source_parent, &destination_parent]
        .into_iter()
        .all(|path| path.as_ref().is_some_and(|path| path.is_absolute()));
    let has_parent_routes = source.as_ref().and_then(|path| path.parent()) == source_parent.as_deref()
        && destination.as_ref().and_then(|path| path.parent()) == destination_parent.as_deref()
        && backup.as_ref().and_then(|path| path.parent()) == destination_parent.as_deref();
    let paths_are_distinct = ledger.source != ledger.destination
        && ledger.source != ledger.backup
        && ledger.destination != ledger.backup;
    let overlaps = source.as_ref().zip(destination.as_ref()).is_some_and(|(source, destination)| {
        path_is_ancestor_or_same(source, destination) || path_is_ancestor_or_same(destination, source)
    });
    if attempt.kind != JournalKind::Move
        || attempt.sources.len() != 1
        || ledger.item != 0
        || attempt.sources.first() != Some(&ledger.source)
        || attempt.destination.as_ref() != Some(&ledger.destination_parent)
        || !paths_are_absolute
        || !has_parent_routes
        || !paths_are_distinct
        || overlaps
        || ledger.source_identity.is_empty()
        || ledger.approved_target_identity.is_empty()
        || ledger.source_identity == ledger.approved_target_identity
        || ledger.volume_identity.is_empty()
        || ledger.source_parent_identity.is_empty()
        || ledger.destination_parent_identity.is_empty()
        || ledger.approved_target_children.len() > MAX_DIRECTORY_ENTRIES
    {
        return Err("same-volume directory Move Replace ledger has invalid attempt, paths, or identities".into());
    }

    let mut previous_depth = usize::MAX;
    let mut previous_path: Option<Vec<String>> = None;
    let mut relative_name_bytes = 0usize;
    let mut cleanup_started = false;
    for (index, child) in ledger.approved_target_children.iter().enumerate() {
        if child.ordinal as usize != index
            || child.identity.is_empty()
            || child.content_fact.is_empty()
            || child.node_kind == JournalDirectoryNodeKind::Unknown
            || child.relative_components.is_empty()
            || child.relative_components.len() > MAX_DIRECTORY_DEPTH
            || child.relative_components.iter().any(|component| {
                component.is_empty()
                    || component == &[b'.' as u16]
                    || component == &[b'.' as u16, b'.' as u16]
                    || component.iter().any(|unit| matches!(*unit, 0 | 0x2f | 0x5c))
            })
        {
            return Err("same-volume directory Move Replace cleanup manifest is malformed".into());
        }
        relative_name_bytes = relative_name_bytes.saturating_add(
            child.relative_components.iter().map(|part| part.len().saturating_mul(2)).sum::<usize>(),
        );
        let depth = child.relative_components.len();
        let path = child.relative_components.iter().map(|part| String::from_utf16_lossy(part).to_lowercase()).collect::<Vec<_>>();
        if depth > previous_depth || (depth == previous_depth && previous_path.as_ref().is_some_and(|previous| previous >= &path)) {
            return Err("same-volume directory Move Replace cleanup manifest is not deepest-first and sorted".into());
        }
        previous_depth = depth;
        previous_path = Some(path);
        if !child.disposition_recorded {
            cleanup_started = true;
        } else if cleanup_started {
            return Err("same-volume directory Move Replace cleanup receipts are not a prefix".into());
        }
    }
    if relative_name_bytes > MAX_DIRECTORY_RELATIVE_NAME_BYTES {
        return Err("same-volume directory Move Replace cleanup manifest exceeds its name limit".into());
    }
    if serde_json::to_vec(ledger).map_err(|error| error.to_string())?.len() > MAX_DIRECTORY_ATTEMPT_BYTES {
        return Err("same-volume directory Move Replace ledger exceeds its serialized size limit".into());
    }
    let all_children_disposed = ledger
        .approved_target_children
        .iter()
        .all(|child| child.disposition_recorded);
    let any_child_disposed = ledger
        .approved_target_children
        .iter()
        .any(|child| child.disposition_recorded);
    if ledger.backup_root_disposed && !all_children_disposed {
        return Err("same-volume directory Move Replace backup root is disposed before its children".into());
    }
    match (&ledger.phase, &ledger.pending_cleanup) {
        (Phase::BackupCleanup, Some(intent)) if intent.ordinal as usize == ledger.approved_target_children.len() => {
            if intent.node_kind != JournalDirectoryNodeKind::Directory
                || !intent.relative_components.is_empty()
                || intent.identity != ledger.approved_target_identity
                || intent.content_fact != "root"
                || !all_children_disposed
                || ledger.backup_root_disposed
            {
                return Err("same-volume directory Move Replace root cleanup intent is invalid".into());
            }
        }
        (Phase::BackupCleanup, Some(intent)) => {
            let Some(child) = ledger.approved_target_children.get(intent.ordinal as usize) else {
                return Err("same-volume directory Move Replace cleanup intent ordinal is invalid".into());
            };
            if child.disposition_recorded
                || ledger.approved_target_children[..intent.ordinal as usize]
                    .iter()
                    .any(|child| !child.disposition_recorded)
                || child.node_kind != intent.node_kind
                || child.relative_components != intent.relative_components
                || child.identity != intent.identity
                || child.content_fact != intent.content_fact
                || ledger.backup_root_disposed
            {
                return Err("same-volume directory Move Replace cleanup intent differs from manifest".into());
            }
        }
        (_, Some(_)) => {
            return Err("same-volume directory Move Replace cleanup intent is out of phase".into());
        }
        _ => {}
    }

    let location_matches_phase = match ledger.phase {
        Phase::Prepared | Phase::NoEffect => {
            ledger.source_location == Source::Original
                && ledger.target_location == Target::Destination
                && !ledger.backup_root_disposed
                && !any_child_disposed
        }
        Phase::DisplaceIntent => ledger.source_location == Source::Original
            && ledger.target_location == Target::Uncertain
            && !ledger.backup_root_disposed
            && !any_child_disposed,
        Phase::TargetDisplaced => ledger.source_location == Source::Original
            && ledger.target_location == Target::Backup
            && !ledger.backup_root_disposed
            && !any_child_disposed,
        Phase::PublishIntent => ledger.source_location == Source::Uncertain
            && ledger.target_location == Target::Backup
            && !ledger.backup_root_disposed
            && !any_child_disposed,
        Phase::DestinationPublished => ledger.source_location == Source::Published
            && ledger.target_location == Target::Backup
            && !ledger.backup_root_disposed
            && !any_child_disposed,
        Phase::BackupCleanup => ledger.source_location == Source::Published
            && ledger.target_location
                == if ledger.backup_root_disposed { Target::Disposed } else { Target::Backup },
        Phase::BackupComplete | Phase::Complete => ledger.source_location == Source::Published
            && ledger.target_location == Target::Disposed
            && ledger.backup_root_disposed
            && ledger.pending_cleanup.is_none()
            && all_children_disposed,
    };
    if !location_matches_phase {
        return Err("same-volume directory Move Replace locations do not match its durable phase".into());
    }
    Ok(())
}

fn path_is_ancestor_or_same(parent: &std::path::Path, child: &std::path::Path) -> bool {
    let parent = parent.components().map(|part| part.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    let child = child.components().map(|part| part.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    parent.len() <= child.len() && parent.iter().zip(&child).all(|(left, right)| left == right)
}

fn validate_directory_move_replace(
    attempt: &JournalAttempt,
    ledger: &JournalDirectoryMove,
    replace: &JournalDirectoryMoveReplace,
) -> Result<(), String> {
    let backup = attempt.replace_backup.as_ref();
    let mut backup_name_bytes = 0usize;
    for child in &replace.backup_children {
        backup_name_bytes = backup_name_bytes.saturating_add(
            child
                .relative_components
                .iter()
                .map(|component| component.len().saturating_mul(2))
                .sum::<usize>(),
        );
    }
    if attempt.kind != JournalKind::Move
        || attempt.sources.len() != 1
        || attempt.directory_moves.len() != 1
        || ledger.item != 0
        || replace.stage.units.is_empty()
        || replace.backup.units.is_empty()
        || replace.stage == ledger.destination
        || replace.backup == ledger.destination
        || replace.stage_identity.is_empty()
        || replace.approved_target_identity.is_empty()
        || replace.approved_target_identity != replace.backup_root_identity
        || replace.destination_parent_identity.is_empty()
        || replace.backup_root_identity.is_empty()
        || backup.map_or(true, |backup| {
            backup.source != ledger.source
                || backup.destination != ledger.destination
                || backup.backup != replace.backup
        })
        || replace.backup_children.len() > MAX_DIRECTORY_ENTRIES
        || backup_name_bytes > MAX_DIRECTORY_RELATIVE_NAME_BYTES
        || replace
            .backup_children
            .iter()
            .enumerate()
            .any(|(index, child)| {
                child.ordinal as usize != index
                    || child.identity.is_empty()
                    || child.content_fact.is_empty()
                    || child.node_kind == JournalDirectoryNodeKind::Unknown
                    || child.relative_components.is_empty()
                    || child.relative_components.len() > MAX_DIRECTORY_DEPTH
                    || child.relative_components.iter().any(|component| {
                        component.is_empty()
                            || component.iter().any(|unit| {
                                *unit == 0 || *unit == b'/' as u16 || *unit == b'\\' as u16
                            })
                    })
            })
        || replace
            .backup_children
            .windows(2)
            .any(|pair| pair[0].relative_components.len() < pair[1].relative_components.len())
    {
        return Err("invalid directory Move Replace record".into());
    }
    if let Some(intent) = &replace.backup_pending {
        let root_intent = intent.ordinal as usize == replace.backup_children.len()
            && intent.node_kind == JournalDirectoryNodeKind::Directory
            && intent.relative_components.is_empty()
            && intent.identity == replace.backup_root_identity
            && intent.content_fact == "root";
        if !root_intent {
            let child = replace
                .backup_children
                .get(intent.ordinal as usize)
                .ok_or_else(|| "invalid directory Move Replace pending ordinal".to_string())?;
            if child.disposition_recorded
                || child.node_kind != intent.node_kind
                || child.relative_components != intent.relative_components
                || child.identity != intent.identity
                || child.content_fact != intent.content_fact
            {
                return Err("directory Move Replace intent differs from backup manifest".into());
            }
        } else if replace
            .backup_children
            .iter()
            .any(|child| !child.disposition_recorded)
        {
            return Err("directory Move Replace backup root intent precedes its children".into());
        }
    }
    let source_complete = ledger.cleanup_complete;
    let source_cleanup_started = ledger.pending.is_some()
        || ledger
            .root
            .as_ref()
            .is_some_and(|root| root.pending.is_some() || root.disposition_recorded)
        || ledger
            .children
            .iter()
            .any(|child| child.disposition_recorded)
        || source_complete;
    let backup_cleanup_started = replace.backup_pending.is_some()
        || replace.backup_root_disposition_recorded
        || replace
            .backup_children
            .iter()
            .any(|child| child.disposition_recorded)
        || replace.backup_cleanup_complete;
    let backup_complete = replace.backup_cleanup_complete
        && replace.backup_root_disposition_recorded
        && replace.backup_pending.is_none()
        && replace
            .backup_children
            .iter()
            .all(|child| child.disposition_recorded);
    let valid_phase = match replace.phase {
        DirectoryMoveReplacePhase::Staged
        | DirectoryMoveReplacePhase::DisplaceIntent
        | DirectoryMoveReplacePhase::StageDiscarded => {
            !source_cleanup_started && !backup_cleanup_started
        }
        DirectoryMoveReplacePhase::TargetDisplaced
        | DirectoryMoveReplacePhase::PublishIntent
        | DirectoryMoveReplacePhase::DestinationPublished => {
            !source_cleanup_started && !backup_cleanup_started
        }
        DirectoryMoveReplacePhase::SourceCleanup => !backup_cleanup_started,
        DirectoryMoveReplacePhase::SourceComplete => source_complete && !backup_complete,
        DirectoryMoveReplacePhase::BackupCleanup => source_complete && !backup_complete,
        DirectoryMoveReplacePhase::BackupComplete => source_complete && backup_complete,
        DirectoryMoveReplacePhase::Complete => source_complete && backup_complete,
    };
    if !valid_phase
        || (source_cleanup_started
            && !matches!(
                replace.phase,
                DirectoryMoveReplacePhase::SourceCleanup
                    | DirectoryMoveReplacePhase::SourceComplete
                    | DirectoryMoveReplacePhase::BackupCleanup
                    | DirectoryMoveReplacePhase::BackupComplete
                    | DirectoryMoveReplacePhase::Complete
            ))
        || (backup_cleanup_started && !source_complete)
        || (replace.backup_cleanup_complete && !source_complete)
        || (replace.backup_root_disposition_recorded
            && replace
                .backup_children
                .iter()
                .any(|child| !child.disposition_recorded))
        || (replace.phase == DirectoryMoveReplacePhase::Complete && !backup_complete)
    {
        return Err("directory Move Replace phase disagrees with cleanup receipts".into());
    }
    Ok(())
}

fn validate_copy_directory_publication_evidence(attempt: &JournalAttempt) -> Result<(), String> {
    let evidence = &attempt.copy_directory_publications;
    if evidence.is_empty() {
        return Ok(());
    }
    if attempt.kind != JournalKind::Copy
        || attempt.state != JournalState::Terminal
        || attempt.terminal_outcome != Some(JournalTerminalOutcome::Unknown)
        || !attempt.receipts_recorded
        || evidence.len() > 64
    {
        return Err("directory Copy publication evidence requires a bounded terminal Unknown Copy".into());
    }
    let mut previous_item = None;
    for record in evidence {
        if previous_item.is_some_and(|item| item >= record.item)
            || record.stage_path.units.is_empty()
            || record.destination_path.units.is_empty()
            || record.stage_path.units.len() > 32_768
            || record.destination_path.units.len() > 32_768
            || record.expected_root_identity.is_empty()
            || record.expected_root_identity.len() > 128
            || record.expected_node_count > 1_000_000
            || record.expected_inventory_sha256.len() != 64
            || !record.expected_inventory_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || record.detail.len() > 1_536
            || record.observed_root_identity.as_ref().is_some_and(|identity| identity.len() > 128)
            || record.observed_inventory_sha256.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            || record.observed_node_count.is_some_and(|count| count > 1_000_000)
            || record.observed_node_count.is_some() != record.observed_inventory_sha256.is_some()
        {
            return Err("directory Copy publication evidence is malformed or over its field bounds".into());
        }
        previous_item = Some(record.item);
    }
    let encoded = serde_json::to_vec(attempt).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_DIRECTORY_ATTEMPT_BYTES {
        return Err("directory Copy publication evidence exceeds its serialized size limit".into());
    }
    Ok(())
}

fn validate_copy_replace_cleanup_vector(attempt: &JournalAttempt) -> Result<(), String> {
    if attempt
        .copy_replace_cleanups
        .windows(2)
        .any(|pair| pair[0].item >= pair[1].item)
    {
        return Err("Copy Replace cleanup items must be unique and sorted".into());
    }
    let backup = attempt.replace_backup.as_ref();
    for ledger in &attempt.copy_replace_cleanups {
        if attempt.kind != JournalKind::Copy
            || ledger.item as usize >= attempt.sources.len()
            || backup.is_some_and(|backup| {
                backup.destination != ledger.destination || backup.backup != ledger.backup
            })
            || (backup.is_none() && !ledger.cleanup_complete)
            || ledger.root_identity.is_empty()
            || ledger.children.iter().enumerate().any(|(index, child)| {
                child.ordinal as usize != index
                    || child.identity.is_empty()
                    || child.content_fact.is_empty()
                    || child.node_kind == JournalDirectoryNodeKind::Unknown
                    || child.relative_components.is_empty()
                    || child.relative_components.iter().any(|component| {
                        component.is_empty()
                            || component.iter().any(|unit| {
                                *unit == 0 || *unit == b'/' as u16 || *unit == b'\\' as u16
                            })
                    })
            })
            || ledger
                .children
                .windows(2)
                .any(|pair| pair[0].relative_components.len() < pair[1].relative_components.len())
        {
            return Err("invalid Copy Replace cleanup manifest".into());
        }
        if let Some(intent) = &ledger.pending {
            let root_intent = intent.ordinal as usize == ledger.children.len()
                && intent.node_kind == JournalDirectoryNodeKind::Directory
                && intent.relative_components.is_empty()
                && intent.identity == ledger.root_identity
                && intent.content_fact == "root";
            if !root_intent {
                let child = ledger
                    .children
                    .get(intent.ordinal as usize)
                    .ok_or_else(|| "invalid Copy Replace pending ordinal".to_string())?;
                if child.disposition_recorded
                    || child.node_kind != intent.node_kind
                    || child.relative_components != intent.relative_components
                    || child.identity != intent.identity
                    || child.content_fact != intent.content_fact
                {
                    return Err("Copy Replace pending intent differs from manifest".into());
                }
            } else if ledger
                .children
                .iter()
                .any(|child| !child.disposition_recorded)
            {
                return Err("Copy Replace root intent precedes descendants".into());
            }
        }
        if ledger.cleanup_complete
            && (ledger.pending.is_some()
                || !ledger.root_disposition_recorded
                || ledger
                    .children
                    .iter()
                    .any(|child| !child.disposition_recorded))
        {
            return Err("completed Copy Replace cleanup has unresolved dispositions".into());
        }
    }
    if !attempt.copy_replace_cleanups.is_empty()
        && serde_json::to_vec(attempt)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_DIRECTORY_ATTEMPT_BYTES
    {
        return Err("Copy Replace cleanup attempt exceeds its serialized size limit".into());
    }
    Ok(())
}

fn validate_directory_children(ledger: &JournalDirectoryMove) -> Result<(), String> {
    if ledger.children.len() > MAX_DIRECTORY_ENTRIES {
        return Err("directory move manifest exceeds its entry limit".into());
    }
    let mut relative_name_bytes = 0usize;
    for (index, child) in ledger.children.iter().enumerate() {
        if child.ordinal as usize != index
            || child.relative_components.is_empty()
            || child.relative_components.len() > MAX_DIRECTORY_DEPTH
            || child.relative_components.iter().any(|component| {
                component.is_empty()
                    || component
                        .iter()
                        .any(|unit| *unit == 0 || *unit == b'/' as u16 || *unit == b'\\' as u16)
            })
        {
            return Err("directory move manifest has invalid child ordinals or names".into());
        }
        relative_name_bytes = relative_name_bytes.saturating_add(
            child
                .relative_components
                .iter()
                .map(|component| component.len().saturating_mul(2))
                .sum::<usize>(),
        );
        if relative_name_bytes > MAX_DIRECTORY_RELATIVE_NAME_BYTES {
            return Err("directory move manifest exceeds its encoded name limit".into());
        }
    }
    let bytes = serde_json::to_vec(ledger).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_DIRECTORY_ATTEMPT_BYTES {
        return Err("directory move attempt exceeds its serialized size limit".into());
    }
    Ok(())
}

pub fn attempt_from_snapshot(job: &JobSnapshot, attempt: &AttemptSnapshot) -> JournalAttempt {
    attempt_from_snapshot_with_key(
        job,
        attempt,
        format!("{}:{}", job.id.value(), attempt.token.attempt_id().value()),
    )
}

pub fn attempt_from_snapshot_for_run(
    job: &JobSnapshot,
    attempt: &AttemptSnapshot,
    run_id: &[u8; 16],
) -> JournalAttempt {
    attempt_from_snapshot_with_key(
        job,
        attempt,
        format!(
            "r:{}:{}:{}",
            hex_run_id(run_id),
            job.id.value(),
            attempt.token.attempt_id().value()
        ),
    )
}

pub fn hex_run_id(run_id: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(32);
    for byte in run_id {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn parse_copy_directory_publications(
    job: &JobSnapshot,
    attempt: &AttemptSnapshot,
) -> Vec<JournalCopyDirectoryPublicationEvidence> {
    let Some(report) = attempt.terminal.as_ref().filter(|report| {
        job.request.kind == OperationKind::Copy
            && report.outcome == super::model::TerminalOutcome::Unknown
    }) else {
        return Vec::new();
    };
    report.receipts.iter().filter_map(|receipt| {
        let detail = receipt.detail.as_deref()?;
        let (_, payload) = detail.split_once(super::model::DIRECTORY_COPY_EVIDENCE_PREFIX)?;
        if payload.len() > 128 * 1024 { return None; }
        let evidence: super::model::CopyDirectoryPublicationEvidence = serde_json::from_str(payload).ok()?;
        if evidence.stage_path_units.is_empty() || evidence.destination_path_units.is_empty()
            || evidence.stage_path_units.len() > 32_768 || evidence.destination_path_units.len() > 32_768
        { return None; }
        Some(JournalCopyDirectoryPublicationEvidence {
            item: receipt.item.0,
            stage_path: JournalPath { units: evidence.stage_path_units },
            destination_path: JournalPath { units: evidence.destination_path_units },
            expected_root_identity: evidence.expected_root_identity,
            expected_node_count: evidence.expected_node_count,
            inventory_kind: JournalCopyDirectoryInventoryKind::CanonicalTreeSha256NoPerEntryRecovery,
            expected_inventory_sha256: evidence.expected_inventory_sha256,
            observed_root_identity: evidence.observed_root_identity,
            observed_node_count: evidence.observed_node_count,
            observed_inventory_sha256: evidence.observed_inventory_sha256,
            phase: match evidence.phase {
                super::model::CopyDirectoryPublicationPhase::RenameOutcomeUncertain => JournalCopyDirectoryPublicationPhase::RenameOutcomeUncertain,
                super::model::CopyDirectoryPublicationPhase::PostPublishCancelled => JournalCopyDirectoryPublicationPhase::PostPublishCancelled,
                super::model::CopyDirectoryPublicationPhase::PublishedRootOpenFailed => JournalCopyDirectoryPublicationPhase::PublishedRootOpenFailed,
                super::model::CopyDirectoryPublicationPhase::PublishedRootIdentityFailed => JournalCopyDirectoryPublicationPhase::PublishedRootIdentityFailed,
                super::model::CopyDirectoryPublicationPhase::PublishedInventoryFailed => JournalCopyDirectoryPublicationPhase::PublishedInventoryFailed,
                super::model::CopyDirectoryPublicationPhase::PublishedInventoryMismatch => JournalCopyDirectoryPublicationPhase::PublishedInventoryMismatch,
            },
            detail: evidence.detail,
        })
    }).collect()
}

fn validate_copy_directory_stage_evidence(attempt: &JournalAttempt) -> Result<(), String> {
    let Some(stage) = &attempt.copy_directory_stage else { return Ok(()); };
    let evidence = &stage.evidence;
    let expected_destination = attempt.destination.as_ref().and_then(|parent| {
        let source = attempt.sources.get(stage.item as usize)?.to_path()?;
        Some(JournalPath::from_path(&parent.to_path()?.join(source.file_name()?)))
    });
    let destination = JournalPath { units: evidence.destination_path_units.clone() };
    let stage_path = JournalPath { units: evidence.stage_path_units.clone() };
    if attempt.kind != JournalKind::Copy
        || attempt.state != JournalState::Terminal
        || attempt.terminal_outcome != Some(JournalTerminalOutcome::Unknown)
        || !attempt.receipts_recorded
        || attempt.sources.len() != 1
        || !attempt.copy_directory_publications.is_empty()
        || !attempt.copy_replace_cleanups.is_empty()
        || !attempt.directory_moves.is_empty()
        || stage.item != 0
        || stage.source != attempt.sources[0]
        || stage.destination != destination
        || expected_destination.as_ref() != Some(&destination)
        || evidence.stage_path_units.is_empty()
        || evidence.destination_path_units.is_empty()
        || evidence.stage_path_units.len() > 32_768
        || evidence.destination_path_units.len() > 32_768
        || evidence.expected_root_identity.len() > 256
        || (evidence.phase == super::model::CopyDirectoryStagePhase::CreatedRootIdentityUnverified
            && !evidence.expected_root_identity.is_empty())
        || evidence.detail.len() > 1024
        || stage_path == destination
        || stage_path.to_path().and_then(|path| path.parent().map(JournalPath::from_path))
            != destination.to_path().and_then(|path| path.parent().map(JournalPath::from_path))
    {
        return Err("invalid retained directory Copy stage evidence".into());
    }
    let encoded = serde_json::to_vec(stage).map_err(|error| error.to_string())?;
    if encoded.len() > 20 * 1024 {
        return Err("retained directory Copy stage evidence exceeds its size limit".into());
    }
    Ok(())
}

fn parse_copy_directory_stage(
    job: &JobSnapshot,
    attempt: &AttemptSnapshot,
) -> Option<JournalCopyDirectoryStageEvidence> {
    use super::model::{ItemOutcome, OperationKind, TerminalOutcome};
    if job.request.kind != OperationKind::Copy || job.request.sources.len() != 1 {
        return None;
    }
    let report = attempt.terminal.as_ref()?;
    if report.outcome != TerminalOutcome::Unknown || report.receipts.len() != 1 {
        return None;
    }
    let receipt = report.receipts.first()?;
    if receipt.item.0 != 0 || receipt.outcome != ItemOutcome::Unknown {
        return None;
    }
    let detail = receipt.detail.as_deref()?;
    let prefix = super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX;
    if detail.match_indices(prefix).count() != 1
        || detail.contains(super::model::DIRECTORY_COPY_EVIDENCE_PREFIX)
    {
        return None;
    }
    let payload = detail.split_once(prefix)?.1;
    if payload.len() > 16 * 1024 {
        return None;
    }
    let evidence: super::model::CopyDirectoryStageEvidence = serde_json::from_str(payload).ok()?;
    if evidence.stage_path_units.is_empty()
        || evidence.destination_path_units.is_empty()
        || evidence.stage_path_units.len() > 32_768
        || evidence.destination_path_units.len() > 32_768
        || evidence.expected_root_identity.len() > 256
        || (evidence.phase == super::model::CopyDirectoryStagePhase::CreatedRootIdentityUnverified
            && !evidence.expected_root_identity.is_empty())
        || evidence.detail.len() > 1024
    {
        return None;
    }
    let source = JournalPath::from_path(&job.request.sources[0]);
    let request_destination = job.request.destination.as_deref()?;
    let destination = JournalPath::from_path(
        &request_destination.join(job.request.sources[0].file_name()?),
    );
    let evidence_destination = JournalPath {
        units: evidence.destination_path_units.clone(),
    };
    let stage = JournalPath {
        units: evidence.stage_path_units.clone(),
    };
    if evidence_destination != destination
        || stage == destination
        || stage.to_path()?.parent() != destination.to_path()?.parent()
    {
        return None;
    }
    Some(JournalCopyDirectoryStageEvidence {
        item: receipt.item.0,
        source,
        destination,
        evidence,
    })
}

fn attempt_from_snapshot_with_key(
    job: &JobSnapshot,
    attempt: &AttemptSnapshot,
    key: String,
) -> JournalAttempt {
    JournalAttempt {
        key,
        created_folder_path: attempt.terminal.as_ref().and_then(|report| {
            if job.request.kind != OperationKind::CreateFolder {
                return None;
            }
            report.receipts.iter().find_map(|receipt| {
                (receipt.outcome == super::model::ItemOutcome::Completed)
                    .then(|| {
                        receipt
                            .created_folder_path
                            .as_deref()
                            .map(JournalPath::from_path)
                    })
                    .flatten()
            })
        }),
        created_archive_path: attempt.terminal.as_ref().and_then(|report| {
            report
                .receipts
                .iter()
                .find_map(|receipt| receipt.created_archive_path.as_deref())
                .map(JournalPath::from_path)
        }),
        zip_create: None,
        seven_zip_create: None,
        seven_zip_create_handles: None,
        zip_extract: None,
        zip_extract_items: Vec::new(),
        zip_extract_receipts: Vec::new(),
        seven_zip_extract_batch: None,
        seven_zip_extract_receipts: Vec::new(),
        kind: match job.request.kind {
            OperationKind::Copy => JournalKind::Copy,
            OperationKind::Move => JournalKind::Move,
            OperationKind::ExternalMoveCleanup => JournalKind::Move,
            // The durable row records only a Copy-like recovery fact; the original
            // handle capability is process-local and is never journaled.
            OperationKind::CopyStageCleanup => JournalKind::Copy,
            OperationKind::CreateSymbolicLink => JournalKind::CreateSymbolicLink,
            OperationKind::CreateJunction => JournalKind::CreateJunction,
            OperationKind::CreateFolder => JournalKind::CreateFolder,
            OperationKind::CreateFile => JournalKind::CreateFile,
            OperationKind::CreateShortcut => JournalKind::CreateShortcut,
            OperationKind::CreateZip => JournalKind::CreateZip,
            OperationKind::Create7z => JournalKind::Create7z,
            OperationKind::ExtractZip => JournalKind::ExtractZip,
            OperationKind::Extract7z => JournalKind::Extract7z,
            OperationKind::Trash => JournalKind::Trash,
            OperationKind::Rename => JournalKind::Rename,
            OperationKind::BatchRename => JournalKind::BatchRename,
            OperationKind::BatchRenameUndo => JournalKind::BatchRenameUndo,
        },
        state: match attempt.state {
            super::model::AttemptState::Queued => JournalState::Queued,
            super::model::AttemptState::Preparing => JournalState::Preparing,
            super::model::AttemptState::Running => JournalState::Running,
            super::model::AttemptState::WaitingConflict => JournalState::WaitingConflict,
            super::model::AttemptState::CancelRequested => JournalState::CancelRequested,
            super::model::AttemptState::Terminal => JournalState::Terminal,
        },
        sources: job
            .request
            .sources
            .iter()
            .map(|path| JournalPath::from_path(path))
            .collect(),
        destination: job
            .request
            .destination
            .as_deref()
            .map(JournalPath::from_path),
        batch_rename_plan: job
            .request
            .batch_rename_plan
            .as_ref()
            .map_or_else(Vec::new, |plan| {
                plan.iter()
                    .map(|item| JournalBatchRenameTarget {
                        source: JournalPath::from_path(&item.source),
                        destination: JournalPath::from_path(&item.destination),
                        source_identity: format!("{:?}", item.source_identity),
                        parent_identity: format!("{:?}", item.parent_identity),
                        source_kind: Some(match item.kind {
                            crate::directory::EntryKind::File => JournalBatchRenameKind::File,
                            crate::directory::EntryKind::Folder => JournalBatchRenameKind::Folder,
                        }),
                        reparse_tag: item.reparse_guard.as_ref().map(|guard| guard.reparse_tag),
                    })
                    .collect()
            }),
        batch_rename_completed: attempt.terminal.as_ref().map_or_else(Vec::new, |report| {
            report
                .receipts
                .iter()
                .filter_map(|receipt| {
                    if receipt.outcome != super::model::ItemOutcome::Completed {
                        return None;
                    }
                    let item = job
                        .request
                        .batch_rename_plan
                        .as_ref()?
                        .get(receipt.item.0 as usize)?;
                    Some(JournalBatchRenameReceipt {
                        source: JournalPath::from_path(&item.source),
                        destination: JournalPath::from_path(&item.destination),
                        source_identity: format!("{:?}", item.source_identity),
                        parent_identity: format!("{:?}", item.parent_identity),
                        source_kind: Some(match item.kind {
                            crate::directory::EntryKind::File => JournalBatchRenameKind::File,
                            crate::directory::EntryKind::Folder => JournalBatchRenameKind::Folder,
                        }),
                        reparse_tag: item.reparse_guard.as_ref().map(|guard| guard.reparse_tag),
                    })
                })
                .collect()
        }),
        replace_backup: None,
        cross_volume_move_replace: None,
        directory_move_replace_rename: None,
        rename_stage: None,
        directory_moves: Vec::new(),
        copy_replace_cleanups: Vec::new(),
        copy_directory_publications: parse_copy_directory_publications(job, attempt),
        copy_directory_stage: parse_copy_directory_stage(job, attempt),
        receipts_recorded: attempt.terminal.is_some(),
        virtual_source_name: job
            .request
            .virtual_source
            .as_ref()
            .and_then(|sources| sources.items.first())
            .map(|source| source.name.clone()),
        virtual_source_size: job
            .request
            .virtual_source
            .as_ref()
            .and_then(|sources| sources.items.first())
            .and_then(|source| source.size),
        virtual_sources: job
            .request
            .virtual_source
            .as_ref()
            .into_iter()
            .flat_map(|sources| sources.items.iter())
            .map(|source| JournalVirtualSource {
                name: source.name.clone(),
                size: source.size,
            })
            .collect(),
        virtual_item_status: job
            .request
            .virtual_source
            .as_ref()
            .map_or_else(Vec::new, |sources| {
                let mut status = vec![0_u8; sources.items.len()];
                if let Some(report) = attempt.terminal.as_ref() {
                    for receipt in &report.receipts {
                        if let Some(item_status) = status.get_mut(receipt.item.0 as usize) {
                            *item_status = match receipt.outcome {
                                super::model::ItemOutcome::Completed => 1,
                                super::model::ItemOutcome::Skipped => 2,
                                super::model::ItemOutcome::Failed => 3,
                                super::model::ItemOutcome::CancelledBeforeStart => 4,
                                super::model::ItemOutcome::Partial => 5,
                                super::model::ItemOutcome::Unknown => 6,
                            };
                        }
                    }
                }
                status
            }),
        terminal_outcome: attempt
            .terminal
            .as_ref()
            .map(|terminal| match terminal.outcome {
                super::model::TerminalOutcome::Succeeded => JournalTerminalOutcome::Succeeded,
                super::model::TerminalOutcome::Failed => JournalTerminalOutcome::Failed,
                super::model::TerminalOutcome::Cancelled => JournalTerminalOutcome::Cancelled,
                super::model::TerminalOutcome::Partial => JournalTerminalOutcome::Partial,
                super::model::TerminalOutcome::Unknown => JournalTerminalOutcome::Unknown,
            }),
    }
}

fn decode(bytes: &[u8]) -> Result<JournalDocument, String> {
    if bytes.len() > MAX_BYTES {
        return Err("operation journal exceeds size limit".into());
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .and_then(|schema| u32::try_from(schema).ok())
        .ok_or_else(|| "operation journal schema is missing or out of range".to_string())?;
    let attempts = value
        .get_mut("attempts")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "operation journal attempts are missing".to_string())?;
    if (1..=4).contains(&schema) {
        for attempt in attempts.iter_mut() {
            let Some(ledger) = attempt.get_mut("zip_create") else {
                continue;
            };
            if ledger.is_null() {
                continue;
            }
            let object = ledger
                .as_object_mut()
                .ok_or_else(|| "legacy ZIP create ledger is invalid".to_string())?;
            let has_legacy_source = object.contains_key("source");
            let has_legacy_identity = object.contains_key("source_identity");
            let has_sources = object.contains_key("sources");
            if has_sources && (has_legacy_source || has_legacy_identity) {
                return Err("ZIP create ledger mixes legacy and ordered source fields".into());
            }
            if has_sources {
                return Err("legacy journal schema cannot contain ordered ZIP sources".into());
            }
            if !has_legacy_source || !has_legacy_identity {
                return Err("legacy ZIP create ledger is missing its captured source facts".into());
            }
            let source = object.remove("source").expect("checked legacy source");
            let source_identity = object
                .remove("source_identity")
                .expect("checked legacy source identity");
            object.insert(
                "sources".into(),
                serde_json::Value::Array(vec![serde_json::json!({
                    "source": source,
                    "source_identity": source_identity,
                })]),
            );
        }
    } else if schema == 5 || schema == 6 || schema == 7 || schema == 8 || schema == SCHEMA {
        for attempt in attempts.iter() {
            let Some(ledger) = attempt.get("zip_create") else {
                continue;
            };
            if ledger.is_null() {
                continue;
            }
            let object = ledger
                .as_object()
                .ok_or_else(|| "ZIP create ledger is invalid".to_string())?;
            if object.contains_key("source") || object.contains_key("source_identity") {
                return Err(
                    "current ZIP create ledger contains legacy singular source fields".into(),
                );
            }
        }
    }
    for attempt in attempts.iter() {
        let new_kind = attempt.get("kind").and_then(serde_json::Value::as_str) == Some("extract-7z");
        let has_new_batch = attempt.get("seven_zip_extract_batch").is_some();
        let has_new_receipts = attempt.get("seven_zip_extract_receipts").is_some();
        if schema < 9 && (new_kind || has_new_batch || has_new_receipts) {
            return Err("pre-schema-9 journal cannot contain 7z extraction facts".into());
        }
        let new_ledger = attempt.get("seven_zip_create_handles");
        let old_ledger = attempt.get("seven_zip_create");
        if schema < 8 && new_ledger.is_some_and(|value| !value.is_null()) {
            return Err("pre-schema-8 journal cannot contain the handle-based 7z ledger".into());
        }
        if new_ledger.is_some_and(|value| !value.is_null())
            && old_ledger.is_some_and(|value| !value.is_null())
        {
            return Err("journal attempt cannot mix legacy and handle-based 7z ledgers".into());
        }
    }
    match schema {
        1 => {
            for attempt in attempts {
                let object = attempt
                    .as_object_mut()
                    .ok_or_else(|| "operation journal attempt is invalid".to_string())?;
                if object.contains_key("directory_moves") {
                    return Err("schema 1 cannot contain directory move vectors".into());
                }
                let legacy = object
                    .remove("directory_move")
                    .unwrap_or(serde_json::Value::Null);
                let ledgers = if legacy.is_null() {
                    Vec::new()
                } else {
                    vec![legacy]
                };
                object.insert("directory_moves".into(), serde_json::Value::Array(ledgers));
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        2 => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("directory_move").is_some())
            {
                return Err("schema 2 cannot contain the legacy directory move field".into());
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        3 => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("directory_move").is_some())
            {
                return Err("schema 3 cannot contain the legacy directory move field".into());
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        4 => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("directory_move").is_some())
            {
                return Err("schema 4 cannot contain the legacy directory move field".into());
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        5 => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("zip_extract_items").is_some())
            {
                return Err("schema 5 cannot contain item-keyed ZIP extract ledgers".into());
            }
            for attempt in attempts.iter_mut() {
                let object = attempt
                    .as_object_mut()
                    .ok_or_else(|| "operation journal attempt is invalid".to_string())?;
                object.insert(
                    "zip_extract_items".into(),
                    serde_json::Value::Array(Vec::new()),
                );
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        6 => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("directory_move").is_some())
            {
                return Err("schema 6 cannot contain the legacy directory move field".into());
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        7 | 8 => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("directory_move").is_some())
            {
                return Err("legacy schema cannot contain the legacy directory move field".into());
            }
            value["schema"] = serde_json::Value::from(SCHEMA);
        }
        SCHEMA => {
            if attempts
                .iter()
                .any(|attempt| attempt.get("directory_move").is_some())
            {
                return Err("schema 6 cannot contain the legacy directory move field".into());
            }
        }
        other => return Err(format!("unknown operation journal schema {other}")),
    }
    let document: JournalDocument =
        serde_json::from_value(value).map_err(|error| error.to_string())?;
    if document.schema != SCHEMA {
        return Err("operation journal schema migration failed".into());
    }
    let mut keys = std::collections::HashSet::with_capacity(document.attempts.len());
    for attempt in &document.attempts {
        if !keys.insert(&attempt.key) {
            return Err(format!(
                "operation journal contains duplicate attempt key: {}",
                attempt.key
            ));
        }
    }
    for attempt in &document.attempts {
        validate_directory_move_vector(attempt)?;
        validate_cross_volume_move_replace(attempt)?;
        validate_zip_create(attempt)?;
        validate_seven_zip_create(attempt)?;
        validate_seven_zip_create_handles(attempt)?;
        validate_zip_extract(attempt)?;
        validate_seven_zip_extract_batch(attempt)?;
        validate_copy_directory_stage_evidence(attempt)?;
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_journal_kind_roundtrips_without_serializing_undo_authority() {
        let kind = JournalKind::CreateShortcut;
        let bytes = serde_json::to_vec(&kind).unwrap();
        assert_eq!(serde_json::from_slice::<JournalKind>(&bytes).unwrap(), kind);
        assert_eq!(bytes, br#""create-shortcut""#);
    }
    use std::path::Path;

    fn attempt(key: u64, state: JournalState) -> JournalAttempt {
        JournalAttempt {
            created_folder_path: None,
            created_archive_path: None,
            zip_create: None,
            seven_zip_create: None,
            seven_zip_create_handles: None,
            zip_extract: None,
            zip_extract_items: Vec::new(),
            zip_extract_receipts: Vec::new(),
            seven_zip_extract_batch: None,
            seven_zip_extract_receipts: Vec::new(),
            key: key.to_string(),
            kind: JournalKind::Trash,
            state,
            sources: vec![],
            destination: None,
            batch_rename_plan: Vec::new(),
            batch_rename_completed: Vec::new(),
            replace_backup: None,
            cross_volume_move_replace: None,
            directory_move_replace_rename: None,
            rename_stage: None,
            directory_moves: Vec::new(),
        copy_replace_cleanups: Vec::new(),
        copy_directory_publications: Vec::new(),
        copy_directory_stage: None,
            receipts_recorded: false,
            virtual_source_name: None,
            virtual_source_size: None,
            virtual_sources: Vec::new(),
            virtual_item_status: Vec::new(),
            terminal_outcome: None,
        }
    }

    #[test]
    fn writer_reads_exact_immutable_attempt_snapshots_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("read-only.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store.upsert(attempt(17, JournalState::Terminal)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let writer = JournalWriter::start(store);

        let exact = writer.read_attempt("17".into()).recv_blocking().unwrap().unwrap();
        assert_eq!(exact.unwrap().key, "17");
        assert!(writer
            .read_attempt("wrong-key".into())
            .recv_blocking()
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn writer_read_reports_unavailable_owner() {
        let (sender, receiver) = async_channel::unbounded();
        drop(receiver);
        let writer = JournalWriter { sender };
        assert!(writer.read_attempt("exact".into()).recv_blocking().unwrap().is_err());
    }

    #[test]
    fn extraction_registration_and_cancelled_observation_gates_preserve_store_bytes() {
        let root = tempfile::tempdir().unwrap();
        let envelope = seven_zip_extract_batch::tests::envelope(root.path(), 1);
        let registration = seven_zip_extract_batch_attempt(envelope.clone());
        let path = root.path().join("extract-gates.json");
        let mut store = JournalStore::open(path.clone()).unwrap();

        let mut late = registration.clone();
        let ledger = seven_zip_extract_batch::tests::initial_item(&envelope, 0);
        let mut first_intent = registration.seven_zip_extract_batch.clone().unwrap();
        first_intent.sequence = 2;
        first_intent.items.push(ledger);
        late.seven_zip_extract_batch = Some(first_intent.clone());
        let before = std::fs::read(&path).unwrap_or_default();
        assert!(store.upsert(late.clone()).is_err(), "empty store must reject item intent before registration");
        assert!(store.document.attempts.is_empty());
        assert_eq!(std::fs::read(&path).unwrap_or_default(), before);

        store.upsert(registration.clone()).unwrap();
        store.upsert(late.clone()).unwrap();
        let mut cancelled = late;
        cancelled.state = JournalState::CancelRequested;
        // The exact already-admitted pre-cancel snapshot remains observable after cancellation.
        store.upsert(cancelled.clone()).unwrap();

        let mut aborting = seven_zip_extract_batch::tests::initial_item(&envelope, 0);
        aborting.sequence = 2;
        aborting.aborting = true;
        let mut abort_batch = first_intent;
        abort_batch.sequence = 3;
        abort_batch.items[0] = aborting;
        let mut abort_snapshot = cancelled.clone();
        abort_snapshot.seven_zip_extract_batch = Some(abort_batch.clone());
        store.upsert(abort_snapshot.clone()).unwrap();

        // A forward non-aborting phase cannot pass the cancellation barrier.
        let mut forward = abort_batch;
        forward.sequence = 4;
        forward.items[0].aborting = false;
        forward.items[0].sequence += 1;
        forward.items[0].phase = JournalSevenZipExtractPhase::SpoolOwned;
        forward.items[0].spool_state = JournalSevenZipExtractObjectState::Owned;
        forward.items[0].spool_identity = Some(JournalSevenZipExtractIdentity { volume: 1, file_index: 300 });
        let mut rejected = abort_snapshot.clone();
        rejected.seven_zip_extract_batch = Some(forward);
        let persisted = std::fs::read(&path).unwrap();
        let in_memory = store.document.clone();
        assert!(store.upsert(rejected).is_err());
        assert_eq!(store.document, in_memory);
        assert_eq!(std::fs::read(&path).unwrap(), persisted);

        // One-step accepted abort trace: pending ownership facts can resolve
        // objects that were never created and finish with a clean discard.
        let mut spool_never = abort_snapshot.seven_zip_extract_batch.clone().unwrap();
        spool_never.sequence += 1;
        spool_never.items[0].sequence += 1;
        spool_never.items[0].spool_state = JournalSevenZipExtractObjectState::NeverCreated;
        let mut snapshot = abort_snapshot.clone();
        snapshot.seven_zip_extract_batch = Some(spool_never.clone());
        store.upsert(snapshot.clone()).unwrap();
        let mut tree_never = spool_never.clone();
        tree_never.sequence += 1;
        tree_never.items[0].sequence += 1;
        tree_never.items[0].tree_state = JournalSevenZipExtractObjectState::NeverCreated;
        snapshot.seven_zip_extract_batch = Some(tree_never.clone());
        store.upsert(snapshot.clone()).unwrap();
        let mut discarded = tree_never;
        discarded.sequence += 1;
        discarded.items[0].sequence += 1;
        discarded.items[0].phase = JournalSevenZipExtractPhase::Discarded;
        snapshot.seven_zip_extract_batch = Some(discarded.clone());
        store.upsert(snapshot.clone()).unwrap();
        let mut clean_cancel = snapshot;
        clean_cancel.state = JournalState::Terminal;
        clean_cancel.terminal_outcome = Some(JournalTerminalOutcome::Cancelled);
        clean_cancel.receipts_recorded = true;
        clean_cancel.seven_zip_extract_receipts = vec![JournalSevenZipExtractTerminalReceipt {
            ordinal: 0,
            outcome: JournalSevenZipExtractOutcome::CancelledBeforeStart,
            output: None,
            publication_identity: None,
        }];
        store.upsert(clean_cancel).unwrap();

        // Aborting TreeCreateIntent -> TreeOwned is a valid exact late fact.
        let mut tree_envelope = seven_zip_extract_batch::tests::envelope(root.path(), 1);
        tree_envelope.attempt.run_id = [99; 16];
        let mut tree_record = seven_zip_extract_batch_attempt(tree_envelope.clone());
        store.upsert(tree_record.clone()).unwrap();
        let mut tree_batch = tree_record.seven_zip_extract_batch.clone().unwrap();
        let mut item = seven_zip_extract_batch::tests::initial_item(&tree_envelope, 0);
        tree_batch.sequence += 1;
        tree_batch.items.push(item.clone());
        tree_record.seven_zip_extract_batch = Some(tree_batch.clone());
        store.upsert(tree_record.clone()).unwrap();
        item.sequence += 1;
        item.phase = JournalSevenZipExtractPhase::SpoolOwned;
        item.spool_state = JournalSevenZipExtractObjectState::Owned;
        item.spool_identity = Some(JournalSevenZipExtractIdentity { volume: 1, file_index: 501 });
        tree_batch.sequence += 1;
        tree_batch.items[0] = item.clone();
        tree_record.seven_zip_extract_batch = Some(tree_batch.clone());
        store.upsert(tree_record.clone()).unwrap();
        item.sequence += 1;
        item.phase = JournalSevenZipExtractPhase::SpoolReady;
        item.spool_ready = Some(JournalSevenZipExtractSpoolReady {
            plan_format: item.binding.plan.plan_format,
            plan_sha256: item.binding.plan.sha256,
            length: item.binding.plan.total_expanded_bytes,
        });
        tree_batch.sequence += 1;
        tree_batch.items[0] = item.clone();
        tree_record.seven_zip_extract_batch = Some(tree_batch.clone());
        store.upsert(tree_record.clone()).unwrap();
        item.sequence += 1;
        item.phase = JournalSevenZipExtractPhase::TreeCreateIntent;
        item.tree_state = JournalSevenZipExtractObjectState::CreateIntent;
        tree_batch.sequence += 1;
        tree_batch.items[0] = item.clone();
        tree_record.seven_zip_extract_batch = Some(tree_batch.clone());
        store.upsert(tree_record.clone()).unwrap();
        tree_record.state = JournalState::CancelRequested;
        store.upsert(tree_record.clone()).unwrap();
        item.sequence += 1;
        item.aborting = true;
        tree_batch.sequence += 1;
        tree_batch.items[0] = item.clone();
        tree_record.seven_zip_extract_batch = Some(tree_batch.clone());
        store.upsert(tree_record.clone()).unwrap();
        item.sequence += 1;
        item.phase = JournalSevenZipExtractPhase::TreeOwned;
        item.tree_state = JournalSevenZipExtractObjectState::Owned;
        item.tree_identity = Some(JournalSevenZipExtractIdentity {
            volume: tree_envelope.destination_parent.volume,
            file_index: 502,
        });
        tree_batch.sequence += 1;
        tree_batch.items[0] = item;
        tree_record.seven_zip_extract_batch = Some(tree_batch.clone());
        store.upsert(tree_record.clone()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let memory = store.document.clone();
        tree_batch.sequence += 1;
        tree_batch.items[0].sequence += 1;
        tree_batch.items[0].tree_identity.as_mut().unwrap().file_index += 1;
        tree_record.seven_zip_extract_batch = Some(tree_batch);
        assert!(store.upsert(tree_record).is_err(), "forged late ownership identity is rejected");
        assert_eq!(store.document, memory);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    fn seven_zip_extract_batch_attempt(envelope: JournalSevenZipExtractEnvelope) -> JournalAttempt {
        let mut record = attempt(8, JournalState::Running);
        record.key = format!("r:{}:8:9", hex_run_id(&envelope.attempt.run_id));
        record.kind = JournalKind::Extract7z;
        record.sources = envelope.items.iter().map(|item| item.source_path.clone()).collect();
        record.destination = envelope.items.first().map(|item| item.output.clone());
        record.seven_zip_extract_batch = Some(JournalSevenZipExtractBatch { envelope, sequence: 1, items: Vec::new() });
        record
    }

    #[test]
    fn schema_nine_seven_zip_extract_round_trips_and_pre_nine_injection_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let envelope = seven_zip_extract_batch::tests::envelope(root.path(), 1);
        let mut record = attempt(8, JournalState::Running);
        record.key = format!("r:{}:8:9", hex_run_id(&[7; 16]));
        record.kind = JournalKind::Extract7z;
        record.sources = envelope.items.iter().map(|item| item.source_path.clone()).collect();
        record.destination = Some(envelope.items[0].output.clone());
        record.seven_zip_extract_batch = Some(JournalSevenZipExtractBatch {
            envelope, sequence: 1, items: Vec::new(),
        });
        let path = root.path().join("extract-journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store.upsert(record.clone()).unwrap();
        drop(store);
        let reopened = JournalStore::open(path.clone()).unwrap();
        assert_eq!(reopened.document.schema, SCHEMA);
        assert_eq!(reopened.document.attempts[0].seven_zip_extract_batch, record.seven_zip_extract_batch);

        let mut injected = serde_json::to_value(JournalDocument {
            schema: 8,
            attempts: vec![record.clone()],
        }).unwrap();
        assert!(decode(&serde_json::to_vec(&injected).unwrap()).is_err());
        injected["attempts"][0]["seven_zip_extract_batch"] = serde_json::Value::Null;
        injected["attempts"][0]["kind"] = serde_json::Value::String("copy".into());
        injected["attempts"][0]["seven_zip_extract_receipts"] = serde_json::json!([]);
        assert!(decode(&serde_json::to_vec(&injected).unwrap()).is_err());
        let size = seven_zip_extract_batch_future_size(&record).unwrap();
        assert!(size <= MAX_DIRECTORY_ATTEMPT_BYTES);

        let mut terminal = record.clone();
        terminal.state = JournalState::Terminal;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Failed);
        terminal.receipts_recorded = true;
        terminal.seven_zip_extract_receipts = vec![JournalSevenZipExtractTerminalReceipt {
            ordinal: 0,
            outcome: JournalSevenZipExtractOutcome::Failed,
            output: None,
            publication_identity: None,
        }];
        let terminal_path = root.path().join("terminal-receipts.json");
        let mut terminal_store = JournalStore::open(terminal_path.clone()).unwrap();
        terminal_store.upsert(record.clone()).unwrap();
        terminal_store.upsert(terminal.clone()).unwrap();
        let saved = std::fs::read(&terminal_path).unwrap();
        let mut conflicting = terminal;
        conflicting.seven_zip_extract_receipts[0].outcome = JournalSevenZipExtractOutcome::CancelledBeforeStart;
        assert!(terminal_store.upsert(conflicting).is_err());
        assert_eq!(std::fs::read(&terminal_path).unwrap(), saved,
            "conflicting terminal receipt rejection must preserve original bytes");

        let maximal = seven_zip_extract_batch_attempt(seven_zip_extract_batch::tests::envelope(root.path(), 1024));
        assert!(seven_zip_extract_batch_future_size(&maximal).is_err(),
            "valid max-width 1024-item reservation should be rejected when its projection exceeds 3 MiB");
        let oversize_path = root.path().join("extract-oversize.json");
        let mut oversize_store = JournalStore::open(oversize_path.clone()).unwrap();
        let initial_bytes = std::fs::read(&oversize_path).unwrap_or_default();
        assert!(oversize_store.upsert(maximal).is_err());
        assert_eq!(oversize_store.document.attempts.len(), 0);
        assert_eq!(std::fs::read(&oversize_path).unwrap_or_default(), initial_bytes,
            "oversize registration must fail before bytes or in-memory state change");

        let near = seven_zip_extract_batch_attempt(seven_zip_extract_batch::tests::envelope(root.path(), 1));
        let reserve_path = root.path().join("extract-reservation.json");
        let mut reserve_store = JournalStore::open(reserve_path.clone()).unwrap();
        reserve_store.upsert(near.clone()).unwrap();
        let reserve = seven_zip_future_growth(&near).unwrap();
        let mut unrelated = attempt(99, JournalState::Preparing);
        let mut candidate = reserve_store.document.clone();
        candidate.attempts.push(unrelated.clone());
        let empty_candidate_size = serde_json::to_vec(&candidate).unwrap().len();
        let target = MAX_BYTES - reserve / 2;
        assert!(empty_candidate_size < target);
        unrelated.key = "u".repeat(target - empty_candidate_size);
        let mut candidate = reserve_store.document.clone();
        candidate.attempts.push(unrelated.clone());
        let current_size = serde_json::to_vec(&candidate).unwrap().len();
        assert!(current_size <= MAX_BYTES);
        assert!(current_size.checked_add(reserve).unwrap() > MAX_BYTES);
        let before_attempts = reserve_store.document.attempts.clone();
        let before_bytes = std::fs::read(&reserve_path).unwrap();
        assert!(reserve_store.upsert(unrelated).is_err(),
            "unrelated writes cannot consume bytes reserved by registered extraction");
        assert_eq!(reserve_store.document.attempts, before_attempts,
            "capacity rejection must leave the in-memory document unchanged");
        assert_eq!(std::fs::read(&reserve_path).unwrap(), before_bytes,
            "capacity rejection must leave durable bytes unchanged");
    }

    #[test]
    fn extract_capacity_reservation_covers_valid_phases_receipts_and_escaped_paths() {
        use JournalSevenZipExtractObjectState as ObjectState;
        use JournalSevenZipExtractPhase as Phase;
        let root = tempfile::tempdir().unwrap();
        let mut envelope = seven_zip_extract_batch::tests::envelope(root.path(), 1);
        let path_with_large_parent = |drive: u16, leaf: &str, target_len: usize| {
            let mut units = vec![drive, b':' as u16, b'\\' as u16];
            let suffix: Vec<u16> = format!("\\{leaf}").encode_utf16().collect();
            let parent_units = target_len - units.len() - suffix.len();
            assert!(parent_units > 2);
            // 最長パスを使いつつ、親要素の不対サロゲートも JSON 往復で保持できることを確かめる。
            units.push(0xD800);
            units.extend(std::iter::repeat_n(b'x' as u16, parent_units - 1));
            units.extend(suffix);
            JournalPath { units }
        };
        let max_path = |drive, leaf| path_with_large_parent(drive, leaf, 32_767);
        let shared_parent_len = 32_767 - ".archive.spool".encode_utf16().count();
        let shared_parent = path_with_large_parent(b'C' as u16, "", shared_parent_len);
        let with_leaf = |leaf: &str| {
            let mut units = shared_parent.units.clone();
            units.pop();
            units.extend([b'\\' as u16]);
            units.extend(leaf.encode_utf16());
            JournalPath { units }
        };
        envelope.items[0].source_path = with_leaf("archive.7z");
        envelope.items[0].output = with_leaf("archive");
        envelope.items[0].spool = with_leaf(".archive.spool");
        envelope.items[0].tree = with_leaf(".archive.tree");
        envelope.provider_executable.path = max_path(b'E' as u16, "7z.exe");
        envelope.provider_dll.path = max_path(b'F' as u16, "7z.dll");
        envelope.application_image.path = max_path(b'G' as u16, "filer.exe");
        envelope.provider_revision = u64::MAX;
        envelope.destination_parent.volume = u32::MAX;
        envelope.destination_parent.file_index = u64::MAX - 1;
        envelope.provider_executable.identity.volume = u32::MAX;
        envelope.provider_executable.identity.file_index = u64::MAX - 2;
        envelope.provider_dll.identity.volume = u32::MAX;
        envelope.provider_dll.identity.file_index = u64::MAX - 3;
        envelope.application_image.identity.volume = u32::MAX;
        envelope.application_image.identity.file_index = u64::MAX - 4;
        envelope.items[0].source_identity.volume = u32::MAX;
        envelope.items[0].source_identity.file_index = u64::MAX - 5;
        envelope.attempt.job = u64::MAX;
        envelope.attempt.attempt = u64::MAX - 1;
        assert!(
            envelope.items[0]
                .source_path
                .units
                .iter()
                .any(|unit| (0xD800..=0xDFFF).contains(unit))
        );
        envelope.validate().unwrap();

        let initial = seven_zip_extract_batch::tests::initial_item(&envelope, 0);
        let mut initial = initial;
        initial.binding.source.length = u64::MAX;
        initial.binding.source.last_write_ticks = u64::MAX;
        // 検証上限に達する有効な plan を使う。上限より大きい値は構造的に不正なので容量対照には使えない。
        initial.binding.plan = JournalSevenZipExtractPlan {
            plan_format: 1,
            sha256: [u8::MAX; 32],
            indexed_record_count: 65_536,
            materialized_node_count: 65_536,
            file_count: 65_536,
            total_expanded_bytes: 8 * 1024 * 1024 * 1024,
        };
        initial.validate().unwrap();
        let plan = initial.binding.plan.clone();
        let registration = JournalSevenZipExtractBatch {
            envelope: envelope.clone(),
            sequence: 1,
            items: Vec::new(),
        };
        let mut reservation_attempt = seven_zip_extract_batch_attempt(envelope.clone());
        reservation_attempt.seven_zip_extract_batch = Some(registration);
        reservation_attempt.key = format!(
            "r:{}:{}:{}",
            hex_run_id(&envelope.attempt.run_id),
            envelope.attempt.job,
            envelope.attempt.attempt
        );
        let initial_reserved = seven_zip_extract_batch_future_size(&reservation_attempt).unwrap();
        assert!(initial_reserved <= MAX_DIRECTORY_ATTEMPT_BYTES);
        let identity = |file_index| JournalSevenZipExtractIdentity {
            volume: u32::MAX,
            file_index,
        };
        let mut spool_owned = initial.clone();
        spool_owned.phase = Phase::SpoolOwned;
        spool_owned.spool_state = ObjectState::Owned;
        spool_owned.spool_identity = Some(identity(u64::MAX - 6));
        let mut spool_ready = spool_owned.clone();
        spool_ready.phase = Phase::SpoolReady;
        spool_ready.spool_ready = Some(JournalSevenZipExtractSpoolReady {
            plan_format: plan.plan_format,
            plan_sha256: plan.sha256,
            length: plan.total_expanded_bytes,
        });
        let mut tree_intent = spool_ready.clone();
        tree_intent.phase = Phase::TreeCreateIntent;
        tree_intent.tree_state = ObjectState::CreateIntent;
        let mut tree_owned = tree_intent.clone();
        tree_owned.phase = Phase::TreeOwned;
        tree_owned.tree_state = ObjectState::Owned;
        tree_owned.tree_identity = Some(identity(u64::MAX - 7));
        let tree_ready_receipt = JournalSevenZipExtractTreeReady {
            plan_format: plan.plan_format,
            plan_sha256: plan.sha256,
            materialized_node_count: plan.materialized_node_count,
            file_count: plan.file_count,
            total_verified_bytes: plan.total_expanded_bytes,
        };
        let mut tree_ready = tree_owned.clone();
        tree_ready.phase = Phase::TreeReady;
        tree_ready.tree_ready = Some(tree_ready_receipt.clone());
        let mut spool_cleaned = tree_ready.clone();
        spool_cleaned.phase = Phase::SpoolCleaned;
        spool_cleaned.spool_state = ObjectState::Deleted;
        let mut publish_intent = spool_cleaned.clone();
        publish_intent.phase = Phase::PublishIntent;
        let mut published = publish_intent.clone();
        published.phase = Phase::Published;
        published.tree_state = ObjectState::Published;
        published.publication_identity = published.tree_identity.clone();
        let mut discarded = initial.clone();
        discarded.aborting = true;
        discarded.phase = Phase::Discarded;
        discarded.spool_state = ObjectState::NeverCreated;
        discarded.tree_state = ObjectState::NeverCreated;

        let phases = [
            initial,
            spool_owned,
            spool_ready,
            tree_intent,
            tree_owned,
            tree_ready,
            spool_cleaned,
            publish_intent,
            published.clone(),
            discarded,
        ];
        for (index, mut ledger) in phases.into_iter().enumerate() {
            ledger.sequence = u64::MAX - 1;
            ledger
                .validate()
                .unwrap_or_else(|error| panic!("invalid capacity fixture at phase {index}: {error}"));
            let batch = JournalSevenZipExtractBatch {
                envelope: envelope.clone(),
                sequence: u64::MAX,
                items: vec![ledger],
            };
            batch.validate().unwrap();
            let mut attempt = seven_zip_extract_batch_attempt(envelope.clone());
            attempt.seven_zip_extract_batch = Some(batch);
            attempt.key = format!(
                "r:{}:{}:{}",
                hex_run_id(&envelope.attempt.run_id),
                envelope.attempt.job,
                envelope.attempt.attempt
            );
            attempt.state = JournalState::Running;
            let actual = serde_json::to_vec(&attempt).unwrap().len();
            assert!(
                actual <= initial_reserved,
                "phase {index} exceeds its initial registration reservation"
            );
            let round_trip: JournalAttempt =
                serde_json::from_slice(&serde_json::to_vec(&attempt).unwrap()).unwrap();
            assert_eq!(
                round_trip.seven_zip_extract_batch.unwrap().envelope.items[0]
                    .source_path
                    .units,
                envelope.items[0].source_path.units,
                "JSON escaping preserves lossless path units"
            );
        }

        let maximal = seven_zip_extract_batch_attempt(seven_zip_extract_batch::tests::envelope(
            root.path(),
            MAX_DIRECTORY_ENTRIES,
        ));
        assert!(
            seven_zip_extract_batch_future_size(&maximal).is_err(),
            "maximum width reservation is refused before admission"
        );

        let mut completed = seven_zip_extract_batch_attempt(envelope.clone());
        completed.key = format!(
            "r:{}:{}:{}",
            hex_run_id(&envelope.attempt.run_id),
            envelope.attempt.job,
            envelope.attempt.attempt
        );
        let completed_batch = JournalSevenZipExtractBatch {
            envelope: envelope.clone(),
            sequence: u64::MAX,
            items: vec![JournalSevenZipExtractLedger {
                sequence: u64::MAX,
                ..published
            }],
        };
        completed.seven_zip_extract_batch = Some(completed_batch);
        completed.state = JournalState::Terminal;
        completed.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        completed.receipts_recorded = true;
        completed.seven_zip_extract_receipts = vec![JournalSevenZipExtractTerminalReceipt {
            ordinal: 0,
            outcome: JournalSevenZipExtractOutcome::Completed,
            output: Some(envelope.items[0].output.clone()),
            publication_identity: Some(identity(u64::MAX - 7)),
        }];
        validate_seven_zip_extract_batch(&completed).unwrap();
        assert!(
            serde_json::to_vec(&completed).unwrap().len() <= initial_reserved,
            "maximum-width terminal receipt is covered by the initial reservation"
        );
    }
    fn seven_zip_attempt(key: String, ledger: JournalSevenZipCreate) -> JournalAttempt {
        let mut record = attempt(0, JournalState::Running);
        record.key = key;
        record.kind = JournalKind::Create7z;
        record.sources = ledger
            .sources
            .iter()
            .map(|source| source.source.clone())
            .collect();
        record.destination = Some(ledger.output.clone());
        record.seven_zip_create = Some(ledger);
        record
    }

    fn seven_zip_handle_attempt(
        key: String,
        ledger: JournalSevenZipHandleCreate,
    ) -> JournalAttempt {
        let mut record = attempt(700, JournalState::Running);
        record.key = key;
        record.kind = JournalKind::Create7z;
        record.sources = ledger
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect();
        record.destination = Some(ledger.output.clone());
        record.seven_zip_create_handles = Some(ledger);
        record
    }

    #[test]
    fn handle_ledger_reserves_future_growth_and_legacy_writer_growth_is_frozen() {
        let mut record =
            seven_zip_handle_attempt(String::new(), seven_zip_handle_create::fixture());
        let projection = seven_zip_handle_future_size(&record).unwrap();
        record.key = "x".repeat(MAX_DIRECTORY_ATTEMPT_BYTES - projection - 32);
        assert!(seven_zip_handle_future_size(&record).unwrap() <= MAX_DIRECTORY_ATTEMPT_BYTES);
        let legacy = seven_zip_attempt(
            "legacy-new-write".into(),
            super::seven_zip_create::tests::fixture(),
        );

        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("7z-capacity.json")).unwrap();
        store.upsert(record.clone()).unwrap();
        assert!(
            store.upsert(legacy).is_err(),
            "schema 8 writers cannot create legacy 7z ledgers"
        );
        store.upsert(record.clone()).unwrap();
        let reserve = seven_zip_future_growth(&record).unwrap();
        assert!(reserve > 0);
        let mut unrelated = attempt(99, JournalState::Preparing);
        unrelated.key.clear();
        let mut candidate = store.document.clone();
        candidate.attempts.push(unrelated.clone());
        let empty_key_size = serde_json::to_vec(&candidate).unwrap().len();
        let target_bytes = MAX_BYTES - reserve / 2;
        assert!(empty_key_size < target_bytes);
        unrelated.key = "u".repeat(target_bytes - empty_key_size);
        let mut candidate = store.document.clone();
        candidate.attempts.push(unrelated.clone());
        let current_size = serde_json::to_vec(&candidate).unwrap().len();
        assert!(current_size <= MAX_BYTES);
        assert!(current_size.saturating_add(reserve) > MAX_BYTES);
        assert!(store.upsert(unrelated).is_err());
        assert_eq!(
            store.document.attempts.len(),
            1,
            "failed unrelated upsert cannot consume reserved bytes"
        );

        let mut escaped =
            seven_zip_handle_attempt("escaped-json".into(), seven_zip_handle_create::fixture());
        let escaped_identity = "\n".repeat(1_600_000);
        let escaped_ledger = escaped.seven_zip_create_handles.as_mut().unwrap();
        escaped_ledger.output_parent_identity = escaped_identity;
        let escaped_path = root.path().join("escaped-json.json");
        let mut escaped_store = JournalStore::open(escaped_path).unwrap();
        assert!(
            escaped_store.upsert(escaped).is_err(),
            "JSON escaping must count toward the bounded future projection"
        );

        let mut near_record =
            seven_zip_handle_attempt(String::new(), seven_zip_handle_create::fixture());
        let near_projection = seven_zip_handle_future_size(&near_record).unwrap();
        near_record.key = "n".repeat(MAX_DIRECTORY_ATTEMPT_BYTES - near_projection - 64);
        assert!(seven_zip_handle_future_size(&near_record).unwrap() <= MAX_DIRECTORY_ATTEMPT_BYTES);
        let mut near_store =
            JournalStore::open(root.path().join("near-limit-ledger.json")).unwrap();
        near_store.upsert(near_record.clone()).unwrap();
        let mut current = near_record.seven_zip_create_handles.clone().unwrap();
        fn persist_step(
            store: &mut JournalStore,
            attempt: &mut JournalAttempt,
            current: &mut JournalSevenZipHandleCreate,
            mut next: JournalSevenZipHandleCreate,
        ) {
            next.sequence = current.sequence.checked_add(1).unwrap();
            assert!(next.follows(current));
            next.validate().unwrap();
            attempt.seven_zip_create_handles = Some(next.clone());
            store.upsert(attempt.clone()).unwrap();
            *current = next;
        }
        let mut next = current.clone();
        next.candidate.state = JournalSevenZipHandleObjectState::Owned;
        next.candidate.identity = Some(JournalSevenZipHandleIdentity {
            volume: 1,
            file_index: 2,
        });
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.phase = JournalSevenZipHandlePhase::CandidateReady;
        next.candidate.length = Some(3);
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.phase = JournalSevenZipHandlePhase::StageCreateIntent;
        next.stage.state = JournalSevenZipHandleObjectState::CreateIntent;
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.stage.state = JournalSevenZipHandleObjectState::Owned;
        next.stage.identity = Some(JournalSevenZipHandleIdentity {
            volume: 1,
            file_index: 3,
        });
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.phase = JournalSevenZipHandlePhase::StageReady;
        next.stage.length = Some(3);
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.candidate.state = JournalSevenZipHandleObjectState::DeleteIntent;
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.candidate.state = JournalSevenZipHandleObjectState::Deleted;
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.phase = JournalSevenZipHandlePhase::CandidateCleaned;
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.phase = JournalSevenZipHandlePhase::PublishIntent;
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        let mut next = current.clone();
        next.phase = JournalSevenZipHandlePhase::Published;
        next.stage.state = JournalSevenZipHandleObjectState::Published;
        next.archive_identity = next.stage.identity.clone();
        persist_step(&mut near_store, &mut near_record, &mut current, next);
        assert_eq!(
            near_store.document.attempts[0]
                .seven_zip_create_handles
                .as_ref()
                .unwrap()
                .phase,
            JournalSevenZipHandlePhase::Published
        );
    }

    #[test]
    fn schema_six_migrates_without_fabricating_handle_or_legacy_7z_ledgers() {
        let legacy = serde_json::to_vec(&JournalDocument {
            schema: 6,
            attempts: vec![attempt(61, JournalState::Running)],
        })
        .unwrap();
        let migrated = decode(&legacy).unwrap();
        assert_eq!(migrated.schema, SCHEMA);
        assert!(migrated.attempts[0].seven_zip_create.is_none());
        assert!(migrated.attempts[0].seven_zip_create_handles.is_none());
    }

    #[test]
    fn schema_seven_legacy_7z_is_preserved_and_handle_ledgers_are_rejected_before_schema_eight() {
        assert!(decode(br#"{"schema":4294967304,"attempts":[]}"#).is_err());
        let legacy_ledger = super::seven_zip_create::tests::fixture();
        let legacy_attempt = seven_zip_attempt("legacy-seven".into(), legacy_ledger.clone());
        let schema_seven = serde_json::to_vec(&JournalDocument {
            schema: 7,
            attempts: vec![legacy_attempt.clone()],
        })
        .unwrap();
        let decoded = decode(&schema_seven).unwrap();
        assert_eq!(decoded.schema, SCHEMA);
        assert_eq!(
            decoded.attempts[0].seven_zip_create.as_ref(),
            Some(&legacy_ledger)
        );
        assert!(decoded.attempts[0].seven_zip_create_handles.is_none());

        let root = tempfile::tempdir().unwrap();
        let legacy_path = root.path().join("legacy-seven.json");
        std::fs::write(&legacy_path, &schema_seven).unwrap();
        let mut store = JournalStore::open(legacy_path.clone()).unwrap();
        let mut omitted = legacy_attempt;
        omitted.seven_zip_create = None;
        store.upsert(omitted).unwrap();
        assert_eq!(
            store.document.attempts[0].seven_zip_create.as_ref(),
            Some(&legacy_ledger)
        );
        drop(store);
        let reopened = JournalStore::open(legacy_path).unwrap();
        assert_eq!(
            reopened.document.attempts[0].seven_zip_create.as_ref(),
            Some(&legacy_ledger),
            "schema-7 legacy data remains unchanged after reopen"
        );

        let mut handle_attempt = attempt(88, JournalState::Running);
        let handle_ledger = seven_zip_handle_create::fixture();
        handle_attempt.kind = JournalKind::Create7z;
        handle_attempt.sources = handle_ledger
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect();
        handle_attempt.destination = Some(handle_ledger.output.clone());
        handle_attempt.seven_zip_create_handles = Some(handle_ledger.clone());
        let pre_schema = serde_json::to_value(JournalDocument {
            schema: 7,
            attempts: vec![handle_attempt.clone()],
        })
        .unwrap();
        assert!(decode(&serde_json::to_vec(&pre_schema).unwrap()).is_err());
        handle_attempt.seven_zip_create = Some(legacy_ledger);
        let mixed = serde_json::to_vec(&JournalDocument {
            schema: 8,
            attempts: vec![handle_attempt],
        })
        .unwrap();
        assert!(decode(&mixed).is_err());

        let mut wrong_kind = seven_zip_handle_attempt("wrong-kind".into(), handle_ledger.clone());
        wrong_kind.kind = JournalKind::Copy;
        assert!(
            decode(
                &serde_json::to_vec(&JournalDocument {
                    schema: 8,
                    attempts: vec![wrong_kind],
                })
                .unwrap()
            )
            .is_err()
        );

        let mut incompatible =
            seven_zip_handle_attempt("incompatible".into(), handle_ledger.clone());
        incompatible.zip_create = Some(zip_create_ledger(JournalZipCreatePhase::StagePlanned));
        assert!(
            decode(
                &serde_json::to_vec(&JournalDocument {
                    schema: 8,
                    attempts: vec![incompatible],
                })
                .unwrap()
            )
            .is_err()
        );

        let mut writer_record =
            seven_zip_handle_attempt("handle-writer".into(), handle_ledger.clone());
        let writer_path = root.path().join("handle-writer.json");
        let mut writer = JournalStore::open(writer_path).unwrap();
        writer.upsert(writer_record.clone()).unwrap();
        writer_record.seven_zip_create_handles = None;
        writer.upsert(writer_record.clone()).unwrap();
        assert_eq!(
            writer.document.attempts[0]
                .seven_zip_create_handles
                .as_ref(),
            Some(&handle_ledger),
            "omission preserves an admitted handle ledger"
        );
        let mut mutation = writer.document.attempts[0].clone();
        mutation
            .seven_zip_create_handles
            .as_mut()
            .unwrap()
            .executable_identity
            .push('x');
        assert!(
            writer.upsert(mutation).is_err(),
            "ledger mutation is rejected"
        );
    }

    fn zip_create_ledger(phase: JournalZipCreatePhase) -> JournalZipCreate {
        JournalZipCreate {
            sources: vec![JournalZipCreateSource {
                source: JournalPath {
                    units: vec![67, 58, 92, 115],
                },
                source_identity: "source-id".into(),
            }],
            output: JournalPath {
                units: vec![67, 58, 92, 111],
            },
            stage: JournalPath {
                units: vec![67, 58, 92, 116],
            },
            parent_identity: "parent-id".into(),
            stage_identity: (phase != JournalZipCreatePhase::StagePlanned)
                .then(|| "stage-id".to_owned()),
            output_identity: (phase == JournalZipCreatePhase::Published)
                .then(|| "output-id".to_owned()),
            phase,
        }
    }

    fn zip_extract_ledger(phase: JournalZipExtractPhase) -> JournalZipExtract {
        JournalZipExtract {
            source: JournalPath {
                units: vec![67, 58, 92, 122],
            },
            output: JournalPath {
                units: vec![67, 58, 92, 111],
            },
            stage: JournalPath {
                units: vec![67, 58, 92, 116],
            },
            source_identity: "extract-source-id".into(),
            parent_identity: "extract-parent-id".into(),
            stage_identity: (phase != JournalZipExtractPhase::StagePlanned)
                .then(|| "extract-stage-id".into()),
            phase,
        }
    }

    #[test]
    fn zip_create_phase_transition_preserves_the_full_ordered_source_vector() {
        let mut previous = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
        let mut second = previous.sources[0].clone();
        second.source.units.push(50);
        second.source_identity = "second-source-id".into();
        previous.sources.push(second);

        let mut valid = previous.clone();
        valid.phase = JournalZipCreatePhase::StageOwned;
        valid.stage_identity = Some("stage-id".into());
        assert!(zip_create_transition_is_valid(&previous, &valid));

        let mut reordered = valid.clone();
        reordered.sources.swap(0, 1);
        let mut truncated = valid.clone();
        truncated.sources.pop();
        let mut appended = valid.clone();
        appended.sources.push(appended.sources[0].clone());
        let mut second_identity_changed = valid.clone();
        second_identity_changed.sources[1].source_identity = "forged-second-id".into();
        for invalid in [reordered, truncated, appended, second_identity_changed] {
            assert!(!zip_create_transition_is_valid(&previous, &invalid));
        }
    }

    #[test]
    fn zip_create_ledger_rejects_empty_duplicate_and_incomplete_source_vectors() {
        let mut attempt = attempt(73, JournalState::Running);
        let mut ledger = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
        attempt.kind = JournalKind::CreateZip;
        attempt.sources = ledger
            .sources
            .iter()
            .map(|source| source.source.clone())
            .collect();
        attempt.destination = Some(ledger.output.clone());
        ledger.sources.clear();
        attempt.zip_create = Some(ledger.clone());
        assert!(validate_zip_create(&attempt).is_err());

        ledger = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
        ledger.sources.push(ledger.sources[0].clone());
        attempt.sources = ledger
            .sources
            .iter()
            .map(|source| source.source.clone())
            .collect();
        attempt.zip_create = Some(ledger.clone());
        assert!(validate_zip_create(&attempt).is_err());

        ledger.sources[1].source.units.push(50);
        ledger.sources[1].source_identity.clear();
        attempt.sources = ledger
            .sources
            .iter()
            .map(|source| source.source.clone())
            .collect();
        attempt.zip_create = Some(ledger);
        assert!(validate_zip_create(&attempt).is_err());
    }

    #[test]
    fn zip_extract_transition_keeps_its_single_source_identity_immutable() {
        let previous = zip_extract_ledger(JournalZipExtractPhase::StagePlanned);
        let mut valid = previous.clone();
        valid.phase = JournalZipExtractPhase::StageOwned;
        valid.stage_identity = Some("extract-stage-id".into());
        assert!(zip_extract_transition_is_valid(&previous, &valid));
        let mut changed = valid;
        changed.source_identity = "changed-source-id".into();
        assert!(!zip_extract_transition_is_valid(&previous, &changed));
    }

    #[test]
    fn schema_five_single_extract_round_trips_without_conversion_to_item_ledgers() {
        let mut record = attempt(909, JournalState::Running);
        record.kind = JournalKind::ExtractZip;
        let legacy = zip_extract_ledger(JournalZipExtractPhase::StagePlanned);
        record.sources = vec![legacy.source.clone()];
        record.destination = Some(legacy.output.clone());
        record.zip_extract = Some(legacy.clone());
        let bytes = serde_json::to_vec(&JournalDocument {
            schema: 5,
            attempts: vec![record],
        })
        .unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.schema, SCHEMA);
        assert_eq!(decoded.attempts[0].zip_extract, Some(legacy));
        assert!(decoded.attempts[0].zip_extract_items.is_empty());
        let persisted = serde_json::to_value(decoded).unwrap();
        assert_eq!(
            persisted["attempts"][0]["zip_extract"]["source"],
            serde_json::json!({"units":[67,58,92,122]})
        );
        assert!(persisted["attempts"][0].get("zip_extract_items").is_none());
    }

    #[test]
    fn partial_batch_recovery_uses_each_item_receipt_and_exact_output() {
        let first_source = JournalPath {
            units: "C:\\one.zip".encode_utf16().collect(),
        };
        let second_source = JournalPath {
            units: "C:\\two.zip".encode_utf16().collect(),
        };
        let first_output = JournalPath {
            units: "C:\\one".encode_utf16().collect(),
        };
        let first = JournalZipExtract {
            source: first_source.clone(),
            output: first_output.clone(),
            stage: JournalPath {
                units: "C:\\.stage-one".encode_utf16().collect(),
            },
            source_identity: "one-id".into(),
            parent_identity: "parent-id".into(),
            stage_identity: Some("stage-id".into()),
            phase: JournalZipExtractPhase::Published,
        };
        let mut record = attempt(910, JournalState::Terminal);
        record.kind = JournalKind::ExtractZip;
        record.sources = vec![first_source, second_source];
        record.destination = Some(first_output.clone());
        record.zip_extract_items = vec![JournalZipExtractItem {
            item: 0,
            ledger: first,
        }];
        record.zip_extract_receipts = vec![
            JournalZipExtractReceipt {
                item: 0,
                outcome: JournalZipExtractItemOutcome::Completed,
                output: Some(first_output),
            },
            JournalZipExtractReceipt {
                item: 1,
                outcome: JournalZipExtractItemOutcome::Failed,
                output: None,
            },
        ];
        record.receipts_recorded = true;
        record.terminal_outcome = Some(JournalTerminalOutcome::Partial);
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store.upsert(record.clone()).unwrap();
        assert!(
            store.recoveries().is_empty(),
            "matching item receipt resolves the Published item despite aggregate Partial"
        );

        record.zip_extract_receipts[0].output = Some(JournalPath {
            units: "C:\\wrong".encode_utf16().collect(),
        });
        assert!(
            store.upsert(record).is_err(),
            "a mismatched output receipt cannot resolve the published item"
        );

        let stage_owned = JournalZipExtract {
            source: JournalPath {
                units: "C:\\one.zip".encode_utf16().collect(),
            },
            output: JournalPath {
                units: "C:\\one".encode_utf16().collect(),
            },
            stage: JournalPath {
                units: "C:\\.stage-one".encode_utf16().collect(),
            },
            source_identity: "one-id".into(),
            parent_identity: "parent-id".into(),
            stage_identity: Some("stage-id".into()),
            phase: JournalZipExtractPhase::StageOwned,
        };
        let mut cleanup_failure = attempt(911, JournalState::Terminal);
        cleanup_failure.kind = JournalKind::ExtractZip;
        cleanup_failure.sources = vec![
            stage_owned.source.clone(),
            JournalPath {
                units: "C:\\two.zip".encode_utf16().collect(),
            },
        ];
        cleanup_failure.destination = Some(stage_owned.output.clone());
        cleanup_failure.zip_extract_items = vec![JournalZipExtractItem {
            item: 0,
            ledger: stage_owned.clone(),
        }];
        cleanup_failure.zip_extract_receipts = vec![
            JournalZipExtractReceipt {
                item: 0,
                outcome: JournalZipExtractItemOutcome::Partial,
                output: Some(stage_owned.stage.clone()),
            },
            JournalZipExtractReceipt {
                item: 1,
                outcome: JournalZipExtractItemOutcome::Failed,
                output: None,
            },
        ];
        cleanup_failure.receipts_recorded = true;
        cleanup_failure.terminal_outcome = Some(JournalTerminalOutcome::Partial);
        let mut cleanup_store =
            JournalStore::open(root.path().join("cleanup-journal.json")).unwrap();
        cleanup_store.upsert(cleanup_failure.clone()).unwrap();
        assert!(
            cleanup_store.recoveries().iter().any(
                |recovery| matches!(recovery, RecoveryItem::RecoveryRequired(attempt) if attempt.key == "911"),
            )
        );
        cleanup_failure.zip_extract_receipts[0].output = Some(stage_owned.output.clone());
        assert!(
            cleanup_store.upsert(cleanup_failure).is_err(),
            "stage-owned partial facts must identify the exact retained stage path"
        );
    }

    #[test]
    fn zip_create_ledger_round_trips_exact_facts_and_unknown_publish_is_inspection_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut record = attempt(1001, JournalState::Running);
        record.kind = JournalKind::CreateZip;
        record.sources = vec![
            zip_create_ledger(JournalZipCreatePhase::StagePlanned).sources[0]
                .source
                .clone(),
        ];
        record.destination = Some(
            zip_create_ledger(JournalZipCreatePhase::StagePlanned)
                .output
                .clone(),
        );
        store.upsert(record.clone()).unwrap();

        let planned = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
        let mut early_output = planned.clone();
        early_output.output_identity = Some("too-early-output-id".into());
        record.zip_create = Some(early_output);
        assert!(store.upsert(record.clone()).is_err());
        record.zip_create = Some(planned.clone());
        store.upsert(record.clone()).unwrap();

        let persisted = std::fs::read(&path).unwrap();
        let mut skipped = planned.clone();
        skipped.phase = JournalZipCreatePhase::Published;
        skipped.stage_identity = Some("stage-id".into());
        record.zip_create = Some(skipped);
        assert!(store.upsert(record.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), persisted);

        let mut owned = planned;
        owned.phase = JournalZipCreatePhase::StageOwned;
        owned.stage_identity = Some("stage-id".into());
        record.zip_create = Some(owned.clone());
        store.upsert(record.clone()).unwrap();
        let mut changed_stage_identity = owned.clone();
        changed_stage_identity.phase = JournalZipCreatePhase::PublishIntent;
        changed_stage_identity.stage_identity = Some("different-stage-id".into());
        record.zip_create = Some(changed_stage_identity);
        assert!(store.upsert(record.clone()).is_err());
        let mut publish = owned;
        publish.phase = JournalZipCreatePhase::PublishIntent;
        record.zip_create = Some(publish.clone());
        store.upsert(record.clone()).unwrap();
        assert!(matches!(
            store.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(_)]
        ));
        drop(store);

        let mut reopened = JournalStore::open(path.clone()).unwrap();
        let RecoveryItem::RecoveryRequired(recovery) = &reopened.recoveries()[0] else {
            panic!("publish intent must require inspection")
        };
        assert_eq!(recovery.zip_create.as_ref(), Some(&publish));
        assert_eq!(recovery.destination.as_ref(), Some(&publish.output));

        let mut published = publish;
        published.phase = JournalZipCreatePhase::Published;
        let mut missing_output_identity = record.clone();
        missing_output_identity.zip_create = Some(published.clone());
        assert!(
            reopened.upsert(missing_output_identity).is_err(),
            "new Published requires output identity"
        );
        let mut empty_output_identity = record.clone();
        published.output_identity = Some(String::new());
        empty_output_identity.zip_create = Some(published.clone());
        assert!(reopened.upsert(empty_output_identity).is_err());
        published.output_identity = Some("output-id".into());
        let mut terminal = record;
        terminal.zip_create = Some(published);
        terminal.state = JournalState::Terminal;
        terminal.receipts_recorded = true;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        reopened.upsert(terminal.clone()).unwrap();
        assert!(reopened.recoveries().is_empty());
        let mut changed_output_identity = terminal.clone();
        changed_output_identity
            .zip_create
            .as_mut()
            .unwrap()
            .output_identity = Some("changed-output-id".into());
        assert!(reopened.upsert(changed_output_identity).is_err());
        drop(reopened);
        let _reopened = JournalStore::open(path.clone()).unwrap();
        let reopened_document = decode(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            reopened_document
                .attempts
                .iter()
                .find(|attempt| attempt.key == terminal.key)
                .unwrap()
                .zip_create,
            terminal.zip_create,
        );
    }

    #[test]
    fn cancelled_zip_create_can_persist_only_exact_stage_discard_cleanup() {
        fn prepared_attempt(key: u64) -> JournalAttempt {
            let mut ledger = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
            ledger.sources.push(JournalZipCreateSource {
                source: JournalPath {
                    units: vec![67, 58, 92, 117],
                },
                source_identity: "source-id-2".into(),
            });
            let mut record = attempt(key, JournalState::Running);
            record.kind = JournalKind::CreateZip;
            record.sources = ledger
                .sources
                .iter()
                .map(|source| source.source.clone())
                .collect();
            record.destination = Some(ledger.output.clone());
            record.zip_create = Some(ledger);
            record
        }

        for (key, previous_phase, actual_stage_identity) in [
            (
                1101,
                JournalZipCreatePhase::StagePlanned,
                "created-after-planned",
            ),
            (1102, JournalZipCreatePhase::StageOwned, "stage-id"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("journal.json");
            let mut store = JournalStore::open(path.clone()).unwrap();
            let mut record = prepared_attempt(key);
            store.upsert(record.clone()).unwrap();
            if previous_phase == JournalZipCreatePhase::StageOwned {
                let mut owned = record.zip_create.clone().unwrap();
                owned.phase = JournalZipCreatePhase::StageOwned;
                owned.stage_identity = Some("stage-id".into());
                record.zip_create = Some(owned);
                store.upsert(record.clone()).unwrap();
            }
            record.state = JournalState::CancelRequested;
            store.upsert(record.clone()).unwrap();
            let mut discarded = record.zip_create.clone().unwrap();
            discarded.phase = JournalZipCreatePhase::StageDiscarded;
            discarded.stage_identity = Some(actual_stage_identity.into());
            record.zip_create = Some(discarded.clone());
            store.upsert(record.clone()).unwrap();
            let durable = std::fs::read(&path).unwrap();
            drop(store);

            let mut reopened = JournalStore::open(path.clone()).unwrap();
            let persisted = reopened
                .document
                .attempts
                .iter()
                .find(|attempt| attempt.key == key.to_string())
                .unwrap();
            assert_eq!(persisted.state, JournalState::CancelRequested);
            assert_eq!(persisted.zip_create.as_ref(), Some(&discarded));

            let mut invalid_cases = Vec::new();
            let mut changed = record.clone();
            changed.zip_create.as_mut().unwrap().sources.swap(0, 1);
            changed.sources = changed
                .zip_create
                .as_ref()
                .unwrap()
                .sources
                .iter()
                .map(|source| source.source.clone())
                .collect();
            invalid_cases.push(changed);
            let mut changed = record.clone();
            changed.zip_create.as_mut().unwrap().sources[1].source_identity =
                "wrong-non-first-id".into();
            invalid_cases.push(changed);
            let mut changed = record.clone();
            changed.zip_create.as_mut().unwrap().output.units.push(1);
            invalid_cases.push(changed);
            let mut changed = record.clone();
            changed
                .zip_create
                .as_mut()
                .unwrap()
                .parent_identity
                .push('x');
            invalid_cases.push(changed);
            let mut changed = record.clone();
            changed.state = JournalState::Running;
            invalid_cases.push(changed);
            let mut changed = record.clone();
            changed.receipts_recorded = true;
            invalid_cases.push(changed);
            let mut changed = record.clone();
            changed.zip_create.as_mut().unwrap().stage_identity = Some(String::new());
            invalid_cases.push(changed);
            if previous_phase == JournalZipCreatePhase::StageOwned {
                let mut changed = record.clone();
                changed.zip_create.as_mut().unwrap().stage_identity = Some("changed-stage".into());
                invalid_cases.push(changed);
            }
            for invalid in invalid_cases {
                assert!(reopened.upsert(invalid).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), durable);
            }
            drop(reopened);
            let reloaded = JournalStore::open(path).unwrap();
            assert_eq!(
                reloaded.document.attempts[0].zip_create.as_ref(),
                Some(&discarded)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal-planned-without-stage.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut record = prepared_attempt(1103);
        store.upsert(record.clone()).unwrap();
        record.state = JournalState::CancelRequested;
        store.upsert(record.clone()).unwrap();
        let mut discarded = record.zip_create.clone().unwrap();
        discarded.phase = JournalZipCreatePhase::StageDiscarded;
        discarded.stage_identity = None;
        record.zip_create = Some(discarded.clone());
        store.upsert(record.clone()).unwrap();
        let durable = std::fs::read(&path).unwrap();
        let mut changed = record.clone();
        changed.zip_create.as_mut().unwrap().stage_identity = Some(String::new());
        assert!(store.upsert(changed).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), durable);
        drop(store);
        let reloaded = JournalStore::open(path).unwrap();
        assert_eq!(
            reloaded.document.attempts[0].zip_create.as_ref(),
            Some(&discarded)
        );
    }

    #[test]
    fn schema_three_published_zip_without_output_identity_loads_as_inspection_history() {
        let mut ledger = zip_create_ledger(JournalZipCreatePhase::Published);
        ledger.output_identity = None;
        let mut record = attempt(7, JournalState::Terminal);
        record.kind = JournalKind::CreateZip;
        record.sources = ledger
            .sources
            .iter()
            .map(|source| source.source.clone())
            .collect();
        record.destination = Some(ledger.output.clone());
        record.created_archive_path = Some(ledger.output.clone());
        record.zip_create = Some(ledger);
        record.receipts_recorded = true;
        record.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        let mut old = serde_json::to_value(JournalDocument {
            schema: 3,
            attempts: vec![record],
        })
        .unwrap();
        let zip = old["attempts"][0]["zip_create"].as_object_mut().unwrap();
        zip.remove("output_identity");
        let legacy_source = zip.remove("sources").unwrap()[0].clone();
        zip.insert("source".into(), legacy_source["source"].clone());
        zip.insert(
            "source_identity".into(),
            legacy_source["source_identity"].clone(),
        );
        let migrated = decode(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(migrated.schema, SCHEMA);
        let ledger = migrated.attempts[0].zip_create.as_ref().unwrap();
        assert_eq!(ledger.phase, JournalZipCreatePhase::Published);
        assert_eq!(ledger.output_identity, None);
        assert!(matches!(migrated.attempts[0].state, JournalState::Terminal));
    }

    #[test]
    fn schema_four_singular_zip_facts_migrate_without_fabrication_and_survive_new_write() {
        for phase in [
            JournalZipCreatePhase::StagePlanned,
            JournalZipCreatePhase::StageOwned,
            JournalZipCreatePhase::PublishIntent,
            JournalZipCreatePhase::Published,
            JournalZipCreatePhase::StageDiscarded,
        ] {
            let ledger = zip_create_ledger(phase);
            let mut record = attempt(71, JournalState::Running);
            record.kind = JournalKind::CreateZip;
            record.sources = ledger
                .sources
                .iter()
                .map(|source| source.source.clone())
                .collect();
            record.destination = Some(ledger.output.clone());
            record.created_archive_path =
                (phase == JournalZipCreatePhase::Published).then(|| ledger.output.clone());
            if phase == JournalZipCreatePhase::Published {
                record.state = JournalState::Terminal;
                record.receipts_recorded = true;
                record.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
            }
            record.zip_create = Some(ledger.clone());
            let mut old = serde_json::to_value(JournalDocument {
                schema: 4,
                attempts: vec![record],
            })
            .unwrap();
            let zip = old["attempts"][0]["zip_create"].as_object_mut().unwrap();
            let source = zip.remove("sources").unwrap()[0].clone();
            zip.insert("source".into(), source["source"].clone());
            zip.insert("source_identity".into(), source["source_identity"].clone());
            let migrated = decode(&serde_json::to_vec(&old).unwrap()).unwrap();
            assert_eq!(migrated.schema, SCHEMA);
            assert_eq!(migrated.attempts[0].zip_create.as_ref().unwrap(), &ledger);

            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("operation-journal.json");
            std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
            let mut store = JournalStore::open(path.clone()).unwrap();
            let mut unrelated = attempt(72, JournalState::Preparing);
            unrelated.key = "72".into();
            store.upsert(unrelated).unwrap();
            drop(store);
            let reloaded = decode(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(reloaded.attempts[0].zip_create.as_ref().unwrap(), &ledger);
            assert_eq!(
                reloaded.attempts[0]
                    .zip_create
                    .as_ref()
                    .unwrap()
                    .sources
                    .len(),
                1
            );
        }
    }

    #[test]
    fn zip_legacy_decoder_skips_null_nonzip_ledgers_rejects_mixed_fields_and_checks_paths() {
        let mut nonzip = serde_json::to_value(JournalDocument {
            schema: 4,
            attempts: vec![attempt(1, JournalState::Preparing)],
        })
        .unwrap();
        nonzip["attempts"][0]["zip_create"] = serde_json::Value::Null;
        assert!(decode(&serde_json::to_vec(&nonzip).unwrap()).is_ok());

        let mut mixed = serde_json::to_value(JournalDocument {
            schema: 4,
            attempts: vec![{
                let mut record = attempt(2, JournalState::Running);
                let ledger = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
                record.kind = JournalKind::CreateZip;
                record.sources = ledger
                    .sources
                    .iter()
                    .map(|source| source.source.clone())
                    .collect();
                record.destination = Some(ledger.output.clone());
                record.zip_create = Some(ledger);
                record
            }],
        })
        .unwrap();
        mixed["attempts"][0]["zip_create"]["source"] =
            serde_json::json!({"units": [67, 58, 92, 115]});
        assert!(decode(&serde_json::to_vec(&mixed).unwrap()).is_err());

        mixed["schema"] = serde_json::Value::from(5);
        mixed["attempts"][0]["zip_create"]["source_identity"] = serde_json::json!("legacy-id");
        assert!(decode(&serde_json::to_vec(&mixed).unwrap()).is_err());

        let mut mismatch_record = attempt(3, JournalState::Running);
        let ledger = zip_create_ledger(JournalZipCreatePhase::StagePlanned);
        mismatch_record.kind = JournalKind::CreateZip;
        mismatch_record.sources = vec![JournalPath {
            units: vec![67, 58, 92, 120],
        }];
        mismatch_record.destination = Some(ledger.output.clone());
        mismatch_record.zip_create = Some(ledger);
        let mismatch = serde_json::to_value(JournalDocument {
            schema: 6,
            attempts: vec![mismatch_record],
        })
        .unwrap();
        assert!(decode(&serde_json::to_vec(&mismatch).unwrap()).is_err());
    }

    #[test]
    fn completed_copy_replace_cleanup_can_outlive_cleared_backup_intent() {
        let destination = JournalPath {
            units: vec![67, 58, 92, 100],
        };
        let backup_path = JournalPath {
            units: vec![67, 58, 92, 98],
        };
        let mut record = attempt(1, JournalState::Terminal);
        record.kind = JournalKind::Copy;
        record.sources.push(JournalPath {
            units: vec![67, 58, 92, 115],
        });
        record.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        record.receipts_recorded = true;
        record
            .copy_replace_cleanups
            .push(JournalCopyReplaceCleanup {
                item: 0,
                destination: destination.clone(),
                backup: backup_path.clone(),
                root_identity: "root-id".into(),
                children: Vec::new(),
                pending: None,
                root_disposition_recorded: true,
                cleanup_complete: true,
            });
        assert!(validate_copy_replace_cleanup_vector(&record).is_ok());

        record.copy_replace_cleanups[0].cleanup_complete = false;
        assert_eq!(
            validate_copy_replace_cleanup_vector(&record),
            Err("invalid Copy Replace cleanup manifest".into())
        );
    }

    fn copy_replace_cleanup_fixture(root: &Path) -> JournalStore {
        let destination = JournalPath {
            units: vec![67, 58, 92, 100],
        };
        let backup = JournalPath {
            units: vec![67, 58, 92, 98],
        };
        let source = JournalPath {
            units: vec![67, 58, 92, 115],
        };
        let ledger = JournalCopyReplaceCleanup {
            item: 0,
            destination: destination.clone(),
            backup: backup.clone(),
            root_identity: "root-id".into(),
            children: Vec::new(),
            pending: None,
            root_disposition_recorded: false,
            cleanup_complete: false,
        };
        let mut record = attempt(1, JournalState::Running);
        record.kind = JournalKind::Copy;
        record.sources = vec![source.clone()];
        record.destination = Some(destination.clone());
        record.replace_backup = Some(JournalReplaceBackup {
            source,
            destination,
            backup,
        });
        let mut store = JournalStore::open(root.join("journal.json")).unwrap();
        store.upsert(record).unwrap();
        store.document.attempts[0]
            .copy_replace_cleanups
            .push(ledger);
        store
    }

    fn root_cleanup_intent(identity: &str) -> JournalCopyReplaceChildIntent {
        JournalCopyReplaceChildIntent {
            ordinal: 0,
            node_kind: JournalDirectoryNodeKind::Directory,
            relative_components: Vec::new(),
            identity: identity.into(),
            content_fact: "root".into(),
        }
    }

    #[test]
    fn copy_replace_cleanup_transition_restores_document_on_validation_and_flush_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut invalid = copy_replace_cleanup_fixture(root.path());
        invalid.document.attempts[0].copy_replace_cleanups[0]
            .root_identity
            .clear();
        let before_validation = invalid.document.clone();
        assert!(
            invalid
                .record_copy_replace_cleanup_intent("1", 0, root_cleanup_intent(""))
                .is_err()
        );
        assert_eq!(invalid.document, before_validation);

        let mut flush_failure = copy_replace_cleanup_fixture(root.path());
        let blocker = root.path().join("file-blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        flush_failure.file = SessionFile::new(SessionPaths::new(blocker.join("journal.json")));
        let before_flush = flush_failure.document.clone();
        assert!(
            flush_failure
                .record_copy_replace_cleanup_intent("1", 0, root_cleanup_intent("root-id"))
                .is_err()
        );
        assert_eq!(flush_failure.document, before_flush);
    }

    #[test]
    fn copy_replace_cleanup_completion_restores_document_on_validation_and_flush_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut invalid = copy_replace_cleanup_fixture(root.path());
        invalid.document.attempts[0].copy_replace_cleanups[0]
            .root_identity
            .clear();
        invalid.document.attempts[0].copy_replace_cleanups[0].root_disposition_recorded = true;
        let before_validation = invalid.document.clone();
        assert!(invalid.complete_copy_replace_cleanup("1", 0).is_err());
        assert_eq!(invalid.document, before_validation);

        let mut flush_failure = copy_replace_cleanup_fixture(root.path());
        flush_failure.document.attempts[0].copy_replace_cleanups[0].root_disposition_recorded =
            true;
        let blocker = root.path().join("file-blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        flush_failure.file = SessionFile::new(SessionPaths::new(blocker.join("journal.json")));
        let before_flush = flush_failure.document.clone();
        assert!(flush_failure.complete_copy_replace_cleanup("1", 0).is_err());
        assert_eq!(flush_failure.document, before_flush);
    }

    fn directory_ledger() -> JournalDirectoryMove {
        JournalDirectoryMove {
            item: 0,
            source: JournalPath {
                units: vec![67, 58],
            },
            destination: JournalPath {
                units: vec![69, 58],
            },
            children: vec![JournalDirectoryChild {
                ordinal: 0,
                node_kind: JournalDirectoryNodeKind::File,
                relative_components: vec![vec![120]],
                source_identity: "source-id".into(),
                destination_identity: "destination-id".into(),
                content_fact: "sha256:abc".into(),
                disposition_recorded: false,
            }],
            pending: None,
            root: Some(JournalDirectoryRoot {
                source_identity: "root-source-id".into(),
                destination_identity: "root-destination-id".into(),
                pending: None,
                disposition_recorded: false,
            }),
            cleanup_complete: false,
            replace: None,
        }
    }

    fn root_intent() -> JournalDirectoryRootIntent {
        JournalDirectoryRootIntent {
            source_identity: "root-source-id".into(),
            destination_identity: "root-destination-id".into(),
        }
    }

    fn move_attempt(key: u64, state: JournalState) -> JournalAttempt {
        let mut attempt = attempt(key, state);
        attempt.kind = JournalKind::Move;
        attempt.sources = vec![JournalPath {
            units: vec![67, 58],
        }];
        attempt
    }

    fn child_intent() -> JournalDirectoryChildIntent {
        JournalDirectoryChildIntent {
            ordinal: 0,
            node_kind: JournalDirectoryNodeKind::File,
            relative_components: vec![vec![120]],
            source_identity: "source-id".into(),
            destination_identity: "destination-id".into(),
            content_fact: "sha256:abc".into(),
        }
    }

    #[test]
    fn directory_move_register_and_transitions_roundtrip_durably() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store
            .upsert(move_attempt(9, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("9", directory_ledger())
            .unwrap();
        store
            .record_directory_move_intent("9", 0, child_intent())
            .unwrap();
        store
            .record_directory_move_receipt("9", 0, child_intent())
            .unwrap();
        store
            .record_directory_move_root_intent("9", 0, root_intent())
            .unwrap();
        store
            .record_directory_move_root_receipt("9", 0, root_intent())
            .unwrap();
        store.complete_directory_move("9", 0).unwrap();
        let mut terminal = move_attempt(9, JournalState::Terminal);
        terminal.receipts_recorded = true;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        store.upsert(terminal).unwrap();
        assert!(store.recoveries().is_empty());
        drop(store);

        let final_store = JournalStore::open(path).unwrap();
        assert!(
            matches!(final_store.document.attempts.as_slice(), [attempt] if attempt.directory_moves[0].children[0].disposition_recorded && attempt.directory_moves[0].pending.is_none() && attempt.directory_moves[0].root.as_ref().unwrap().disposition_recorded && attempt.directory_moves[0].cleanup_complete && attempt.terminal_outcome == Some(JournalTerminalOutcome::Succeeded))
        );
    }

    #[test]
    fn directory_move_rejects_stale_item_facts_and_duplicate_receipts_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(move_attempt(10, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("10", directory_ledger())
            .unwrap();
        let before = store.document.clone();
        assert!(
            store
                .record_directory_move_intent("retry-10", 0, child_intent())
                .is_err()
        );
        assert!(
            store
                .record_directory_move_intent("10", 1, child_intent())
                .is_err()
        );
        let mut wrong = child_intent();
        wrong.content_fact = "sha256:wrong".into();
        assert!(store.record_directory_move_intent("10", 0, wrong).is_err());
        assert_eq!(store.document, before);
        assert!(
            store
                .record_directory_move_receipt("10", 0, child_intent())
                .is_err()
        );
        assert_eq!(store.document, before);
        store
            .record_directory_move_intent("10", 0, child_intent())
            .unwrap();
        store
            .record_directory_move_receipt("10", 0, child_intent())
            .unwrap();
        let after_receipt = store.document.clone();
        assert!(
            store
                .record_directory_move_receipt("10", 0, child_intent())
                .is_err()
        );
        assert_eq!(store.document, after_receipt);
    }

    #[test]
    fn directory_move_root_receipt_gates_completion_and_survives_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store
            .upsert(move_attempt(21, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("21", directory_ledger())
            .unwrap();
        assert!(
            store
                .record_directory_move_root_intent("21", 0, root_intent())
                .is_err()
        );
        store
            .record_directory_move_intent("21", 0, child_intent())
            .unwrap();
        assert!(
            store
                .record_directory_move_root_intent("21", 0, root_intent())
                .is_err()
        );
        store
            .record_directory_move_receipt("21", 0, child_intent())
            .unwrap();
        let mut wrong = root_intent();
        wrong.source_identity = "stale-root".into();
        assert!(
            store
                .record_directory_move_root_intent("21", 0, wrong.clone())
                .is_err()
        );
        store
            .record_directory_move_root_intent("21", 0, root_intent())
            .unwrap();
        let before = store.document.clone();
        assert!(
            store
                .record_directory_move_intent("21", 0, child_intent())
                .is_err()
        );
        assert!(
            store
                .record_directory_move_root_receipt("21", 0, wrong)
                .is_err()
        );
        assert_eq!(store.document, before);
        drop(store);
        let mut recovered = JournalStore::open(path.clone()).unwrap();
        assert!(
            matches!(recovered.recoveries().as_slice(), [RecoveryItem::RecoveryRequired(record)] if record.directory_moves[0].root.as_ref().unwrap().pending.is_some())
        );
        assert!(recovered.complete_directory_move("21", 0).is_err());
        recovered
            .record_directory_move_root_receipt("21", 0, root_intent())
            .unwrap();
        assert!(
            recovered
                .record_directory_move_root_receipt("21", 0, root_intent())
                .is_err()
        );
        recovered.complete_directory_move("21", 0).unwrap();
        assert!(
            JournalStore::open(path).unwrap().document.attempts[0].directory_moves[0]
                .cleanup_complete
        );
    }

    #[test]
    fn directory_move_empty_children_still_requires_root_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(move_attempt(22, JournalState::Running))
            .unwrap();
        let mut ledger = directory_ledger();
        ledger.children.clear();
        store.register_directory_move("22", ledger).unwrap();
        assert!(store.complete_directory_move("22", 0).is_err());
        store
            .record_directory_move_root_intent("22", 0, root_intent())
            .unwrap();
        store
            .record_directory_move_root_receipt("22", 0, root_intent())
            .unwrap();
        store.complete_directory_move("22", 0).unwrap();
    }

    #[test]
    fn legacy_directory_move_without_root_state_cannot_complete() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store
            .upsert(move_attempt(23, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("23", directory_ledger())
            .unwrap();
        let mut legacy = serde_json::to_value(&store.document).unwrap();
        legacy["attempts"][0]["directory_moves"][0]
            .as_object_mut()
            .unwrap()
            .remove("root");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let mut recovered = JournalStore::open(path).unwrap();
        assert!(matches!(
            recovered.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(_)]
        ));
        assert!(recovered.complete_directory_move("23", 0).is_err());
    }

    #[test]
    fn directory_move_writer_acknowledges_flushed_transitions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store
            .upsert(move_attempt(11, JournalState::Running))
            .unwrap();
        let writer = JournalWriter::start(store);
        writer
            .register_directory_move("11".into(), directory_ledger())
            .recv_blocking()
            .unwrap()
            .unwrap();
        writer
            .record_directory_move_intent("11".into(), 0, child_intent())
            .recv_blocking()
            .unwrap()
            .unwrap();
        let reopened = JournalStore::open(path).unwrap();
        assert!(
            reopened.document.attempts[0].directory_moves[0]
                .pending
                .is_some()
        );
    }

    #[test]
    fn only_non_terminal_attempts_require_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store.upsert(attempt(1, JournalState::Running)).unwrap();
        store.upsert(attempt(2, JournalState::Terminal)).unwrap();
        assert!(
            matches!(store.recoveries().as_slice(), [RecoveryItem::RecoveryRequired(value)] if value.key == "1")
        );
    }

    #[test]
    fn batch_rename_partial_and_unknown_terminals_reopen_as_inspection_only_recoveries() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let completed_receipt = JournalBatchRenameReceipt {
            source: JournalPath {
                units: "C:\\old.txt".encode_utf16().collect(),
            },
            destination: JournalPath {
                units: "C:\\new.txt".encode_utf16().collect(),
            },
            source_identity: "recorded-source-identity".into(),
            parent_identity: "recorded-parent-identity".into(),
            source_kind: None,
            reparse_tag: None,
        };
        let cases = [
            (
                31,
                JournalKind::BatchRename,
                JournalTerminalOutcome::Partial,
                true,
            ),
            (
                32,
                JournalKind::BatchRename,
                JournalTerminalOutcome::Unknown,
                true,
            ),
            (
                33,
                JournalKind::BatchRenameUndo,
                JournalTerminalOutcome::Partial,
                true,
            ),
            (
                34,
                JournalKind::BatchRenameUndo,
                JournalTerminalOutcome::Unknown,
                true,
            ),
            (
                35,
                JournalKind::BatchRename,
                JournalTerminalOutcome::Succeeded,
                false,
            ),
            (
                36,
                JournalKind::BatchRename,
                JournalTerminalOutcome::Failed,
                false,
            ),
            (
                37,
                JournalKind::BatchRename,
                JournalTerminalOutcome::Cancelled,
                false,
            ),
            (
                38,
                JournalKind::BatchRenameUndo,
                JournalTerminalOutcome::Succeeded,
                false,
            ),
            (
                39,
                JournalKind::BatchRenameUndo,
                JournalTerminalOutcome::Failed,
                false,
            ),
            (
                40,
                JournalKind::BatchRenameUndo,
                JournalTerminalOutcome::Cancelled,
                false,
            ),
        ];
        for (key, kind, outcome, _) in cases {
            let mut record = attempt(key, JournalState::Terminal);
            record.kind = kind;
            record.receipts_recorded = true;
            record.terminal_outcome = Some(outcome);
            record.batch_rename_completed = vec![completed_receipt.clone()];
            store.upsert(record).unwrap();
        }
        drop(store);

        let reopened = JournalStore::open(path).unwrap();
        let recoveries = reopened.recoveries();
        assert_eq!(recoveries.len(), 4);
        for (key, kind, outcome, should_recover) in cases {
            let persisted = reopened
                .document
                .attempts
                .iter()
                .find(|record| record.key == key.to_string())
                .unwrap();
            assert_eq!(persisted.state, JournalState::Terminal);
            assert_eq!(persisted.terminal_outcome, Some(outcome));
            assert!(persisted.receipts_recorded);
            assert_eq!(
                persisted.batch_rename_completed,
                vec![completed_receipt.clone()]
            );

            let projected = recoveries.iter().find_map(|item| match item {
                RecoveryItem::RecoveryRequired(record) if record.key == key.to_string() => {
                    Some(record)
                }
                _ => None,
            });
            assert_eq!(projected.is_some(), should_recover, "{kind:?} {outcome:?}");
            if let Some(projected) = projected {
                assert_eq!(projected.state, JournalState::RecoveryRequired);
                assert_eq!(projected.terminal_outcome, Some(outcome));
                assert_eq!(
                    projected.batch_rename_completed,
                    vec![completed_receipt.clone()]
                );
            }
        }
    }

    #[test]
    fn replace_backup_survives_a_durable_recovery_required_record() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut record = attempt(1, JournalState::RecoveryRequired);
        record.replace_backup = Some(JournalReplaceBackup {
            source: JournalPath { units: vec![1, 2] },
            destination: JournalPath { units: vec![3, 4] },
            backup: JournalPath { units: vec![5, 6] },
        });
        store.upsert(record).unwrap();
        drop(store);

        let reopened = JournalStore::open(path).unwrap();
        assert!(matches!(
            reopened.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(record)]
                if record.state == JournalState::RecoveryRequired
                    && record.replace_backup.as_ref().is_some_and(|backup| backup.backup.units == [5, 6])
        ));
    }

    #[test]
    fn directory_move_attempt_with_pending_child_intent_requires_recovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut record = move_attempt(3, JournalState::Running);
        record.sources = vec![
            JournalPath {
                units: vec![67, 58]
            };
            3
        ];
        store.upsert(record).unwrap();
        let mut ledger = directory_ledger();
        ledger.item = 2;
        store.register_directory_move("3", ledger).unwrap();
        store
            .record_directory_move_intent("3", 2, child_intent())
            .unwrap();
        drop(store);

        let recovered = JournalStore::open(path).unwrap();
        assert!(matches!(
            recovered.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(record)]
                if record.directory_moves.iter().find(|move_record| move_record.item == 2).is_some_and(|move_record|
                    move_record.pending.as_ref().is_some_and(|intent| intent.ordinal == 0))
        ));
    }

    #[test]
    fn terminal_snapshot_with_pending_directory_intent_recovers_then_accepts_exact_receipt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store
            .upsert(move_attempt(5, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("5", directory_ledger())
            .unwrap();
        store
            .record_directory_move_intent("5", 0, child_intent())
            .unwrap();
        let mut terminal = move_attempt(5, JournalState::Terminal);
        terminal.receipts_recorded = true;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Partial);
        store.upsert(terminal).unwrap();
        assert!(
            matches!(store.recoveries().as_slice(), [RecoveryItem::RecoveryRequired(record)] if record.state == JournalState::RecoveryRequired && record.receipts_recorded)
        );
        drop(store);

        let mut recovered = JournalStore::open(path).unwrap();
        assert!(matches!(
            recovered.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(record)]
                if record.state == JournalState::RecoveryRequired
                    && record.directory_moves.iter().find(|directory_move| directory_move.item == 0).is_some_and(|directory_move|
                        directory_move.pending.as_ref().is_some_and(|intent| intent.ordinal == 0))
        ));
        assert!(
            recovered
                .record_directory_move_receipt("5", 0, child_intent())
                .is_ok()
        );
        assert!(
            matches!(recovered.recoveries().as_slice(), [RecoveryItem::RecoveryRequired(record)] if record.directory_moves[0].pending.is_none() && record.terminal_outcome == Some(JournalTerminalOutcome::Partial))
        );
        assert_eq!(recovered.document.attempts[0].state, JournalState::Terminal);
        assert!(recovered.document.attempts[0].receipts_recorded);
    }

    #[test]
    fn terminal_snapshot_cannot_be_replaced_by_conflicting_terminal_facts() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        let mut terminal = attempt(14, JournalState::Terminal);
        terminal.receipts_recorded = true;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Partial);
        store.upsert(terminal.clone()).unwrap();

        let mut stale = terminal;
        stale.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        assert!(store.upsert(stale).is_err());
        assert_eq!(
            store.document.attempts[0].terminal_outcome,
            Some(JournalTerminalOutcome::Partial)
        );
    }

    #[test]
    fn retained_copy_stage_evidence_is_optional_typed_and_terminal_immutable() {
        use super::super::model::{CopyDirectoryStageEvidence, CopyDirectoryStagePhase};
        let legacy: JournalAttempt = serde_json::from_value(serde_json::json!({
            "key":"legacy", "kind":"copy", "state":"recovery-required",
            "sources":[], "destination":null, "receipts_recorded":false
        })).unwrap();
        assert!(legacy.copy_directory_stage.is_none());

        let source = JournalPath::from_path(Path::new(r"C:\source\folder"));
        let parent = JournalPath::from_path(Path::new(r"C:\target"));
        let destination = JournalPath::from_path(Path::new(r"C:\target\folder"));
        let stage = JournalPath::from_path(Path::new(r"C:\target\.gpui-filer-stage"));
        let evidence = JournalCopyDirectoryStageEvidence {
            item: 0,
            source: source.clone(),
            destination: destination.clone(),
            evidence: CopyDirectoryStageEvidence {
                stage_path_units: stage.units.clone(),
                destination_path_units: destination.units.clone(),
                expected_root_identity: "volume:root-id".into(),
                phase: CopyDirectoryStagePhase::StagingFailed,
                detail: "injected failure".into(),
            },
        };
        let mut terminal = attempt(91, JournalState::Terminal);
        terminal.kind = JournalKind::Copy;
        terminal.sources = vec![source];
        terminal.destination = Some(parent);
        terminal.receipts_recorded = true;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Unknown);
        terminal.copy_directory_stage = Some(evidence.clone());
        validate_copy_directory_stage_evidence(&terminal).unwrap();

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store.upsert(terminal.clone()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let reopened = JournalStore::open(path.clone()).unwrap();
        assert_eq!(reopened.document.attempts[0].copy_directory_stage, Some(evidence));
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut conflict = terminal.clone();
        conflict.copy_directory_stage.as_mut().unwrap().evidence.detail = "changed".into();
        assert!(store.upsert(conflict).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut wrong_outcome = terminal.clone();
        wrong_outcome.terminal_outcome = Some(JournalTerminalOutcome::Failed);
        assert!(validate_copy_directory_stage_evidence(&wrong_outcome).is_err());
        let mut wrong_item = terminal.clone();
        wrong_item.copy_directory_stage.as_mut().unwrap().item = 1;
        assert!(validate_copy_directory_stage_evidence(&wrong_item).is_err());
        let mut identity_unverified_with_identity = terminal.clone();
        let evidence = &mut identity_unverified_with_identity.copy_directory_stage.as_mut().unwrap().evidence;
        evidence.phase = CopyDirectoryStagePhase::CreatedRootIdentityUnverified;
        assert!(validate_copy_directory_stage_evidence(&identity_unverified_with_identity).is_err());
        assert!(store.upsert(identity_unverified_with_identity).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut malformed_document: serde_json::Value = serde_json::from_slice(&before).unwrap();
        malformed_document["attempts"][0]["copy_directory_stage"]["evidence"]["phase"] =
            serde_json::json!("created-root-identity-unverified");
        std::fs::write(&path, serde_json::to_vec(&malformed_document).unwrap()).unwrap();
        assert!(JournalStore::open(path.clone()).is_err(),
            "decode rejects created-but-unverified evidence that claims an identity");
        let mut missing_identity = terminal.clone();
        missing_identity.key = "copy-stage-missing-identity".into();
        missing_identity.copy_directory_stage.as_mut().unwrap().evidence.expected_root_identity.clear();
        validate_copy_directory_stage_evidence(&missing_identity).unwrap();
        store.upsert(missing_identity.clone()).unwrap();
        let reopened = JournalStore::open(path.clone()).unwrap();
        assert!(reopened.document.attempts.iter().any(|attempt| {
            attempt.key == missing_identity.key
                && attempt.copy_directory_stage.as_ref().is_some_and(|stage| stage.evidence.expected_root_identity.is_empty())
        }));
        let mut oversized = terminal;
        oversized.copy_directory_stage.as_mut().unwrap().evidence.detail = "x".repeat(1025);
        assert!(validate_copy_directory_stage_evidence(&oversized).is_err());
    }

    #[test]
    fn retained_copy_stage_projection_requires_one_exact_unknown_receipt_mapping() {
        use std::os::windows::ffi::OsStrExt;
        use super::super::model::{
            CopyDirectoryStageEvidence, CopyDirectoryStagePhase, FsChangeHint, ItemId,
            ItemOutcome, ItemReceipt, OperationKind, OperationRequest, OperationService,
            ProgressCounters, RetryPlan, TerminalOutcome, TerminalReport,
        };
        fn snapshot(item: u32, item_outcome: ItemOutcome, outcome: TerminalOutcome, detail: String,
            requested_destination: &str) -> (JobSnapshot, AttemptSnapshot) {
            let source = Path::new(r"C:\source\folder");
            let destination = Path::new(r"C:\target\folder");
            let stage = Path::new(r"C:\target\.gpui-filer-stage");
            let mut evidence = CopyDirectoryStageEvidence {
                stage_path_units: stage.as_os_str().encode_wide().collect(),
                destination_path_units: destination.as_os_str().encode_wide().collect(),
                expected_root_identity: String::new(),
                phase: CopyDirectoryStagePhase::CreatedRootIdentityUnverified,
                detail: "stage root identity unavailable".into(),
            };
            let forged_identity = detail == "<nonempty-identity>";
            if forged_identity {
                evidence.expected_root_identity = "forged-stage-root-id".into();
            }
            let payload = serde_json::to_string(&evidence).unwrap();
            let detail = if detail == "<payload>" || forged_identity {
                format!("retained stage; {}{payload}", super::super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX)
            } else { detail };
            let mut service = OperationService::default();
            let token = service.submit(OperationRequest {
                kind: OperationKind::Copy,
                virtual_source: None,
                sources: vec![source.to_path_buf()],
                trash_source_guards: None,
                destination: Some(PathBuf::from(requested_destination)),
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
            }).unwrap();
            service.start_next().unwrap();
            service.terminal(token, TerminalReport {
                outcome,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(item), outcome: item_outcome, detail: Some(detail),
                    created_folder_path: None, created_archive_path: None, move_receipt: None,
                    replace_receipt: None, copy_undo: None,
                }],
                retry: RetryPlan::RequiresReinspection,
                change_hints: Vec::<FsChangeHint>::new(),
            }).unwrap();
            let job = service.jobs().pop().unwrap();
            let attempt = job.attempts[0].clone();
            (job, attempt)
        }

        let (job, attempt) = snapshot(0, ItemOutcome::Unknown, TerminalOutcome::Unknown, "<payload>".into(), r"C:\target");
        let projected = attempt_from_snapshot(&job, &attempt);
        assert!(projected.copy_directory_stage.is_some());
        assert!(projected.copy_directory_stage.unwrap().evidence.expected_root_identity.is_empty());
        let (job, attempt) = snapshot(1, ItemOutcome::Unknown, TerminalOutcome::Unknown, "<payload>".into(), r"C:\target");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none());
        let (job, attempt) = snapshot(0, ItemOutcome::Failed, TerminalOutcome::Unknown, "<payload>".into(), r"C:\target");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none());
        let (job, attempt) = snapshot(0, ItemOutcome::Unknown, TerminalOutcome::Failed, "<payload>".into(), r"C:\target");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none());
        let (job, attempt) = snapshot(0, ItemOutcome::Unknown, TerminalOutcome::Unknown, "<payload>".into(), r"C:\elsewhere");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none());
        let (job, attempt) = snapshot(0, ItemOutcome::Unknown, TerminalOutcome::Unknown, "<nonempty-identity>".into(), r"C:\target");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none(),
            "created-but-unverified evidence with a claimed identity is not projected");
        let (job, attempt) = snapshot(0, ItemOutcome::Unknown, TerminalOutcome::Unknown,
            format!("{}not-json", super::super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX), r"C:\target");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none());
        let (job, attempt) = snapshot(0, ItemOutcome::Unknown, TerminalOutcome::Unknown,
            format!("{}{}", super::super::model::DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX, "x".repeat(16 * 1024 + 1)), r"C:\target");
        assert!(attempt_from_snapshot(&job, &attempt).copy_directory_stage.is_none());
    }

    #[test]
    fn generic_upsert_cannot_revive_terminal_attempt_or_clear_receipts() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        let mut terminal = attempt(12, JournalState::Terminal);
        terminal.receipts_recorded = true;
        store.upsert(terminal.clone()).unwrap();

        let mut stale = attempt(12, JournalState::Running);
        assert!(store.upsert(stale.clone()).is_err());
        stale.state = JournalState::Terminal;
        store.upsert(stale).unwrap();
        assert!(store.document.attempts[0].receipts_recorded);
        assert_eq!(store.document.attempts[0].state, JournalState::Terminal);
    }

    #[test]
    fn cancel_requested_attempt_cannot_be_revived_or_accept_a_child_intent() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(move_attempt(15, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("15", directory_ledger())
            .unwrap();
        store
            .upsert(move_attempt(15, JournalState::CancelRequested))
            .unwrap();

        assert!(
            store
                .upsert(move_attempt(15, JournalState::Running))
                .is_err()
        );
        assert!(
            store
                .record_directory_move_intent("15", 0, child_intent())
                .is_err()
        );
        assert_eq!(
            store.document.attempts[0].state,
            JournalState::CancelRequested
        );
        assert!(
            store.document.attempts[0].directory_moves[0]
                .pending
                .is_none()
        );
    }

    #[test]
    fn recovery_required_snapshot_cannot_return_to_running() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(move_attempt(16, JournalState::RecoveryRequired))
            .unwrap();
        assert!(
            store
                .upsert(move_attempt(16, JournalState::Running))
                .is_err()
        );
        assert_eq!(
            store.document.attempts[0].state,
            JournalState::RecoveryRequired
        );
    }

    #[test]
    fn waiting_conflict_can_resume_running() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(attempt(17, JournalState::WaitingConflict))
            .unwrap();
        store.upsert(attempt(17, JournalState::Running)).unwrap();
        assert_eq!(store.document.attempts[0].state, JournalState::Running);
    }

    #[test]
    fn directory_move_registration_rejects_already_disposed_manifest_children() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(move_attempt(13, JournalState::Running))
            .unwrap();
        let mut ledger = directory_ledger();
        ledger.children[0].node_kind = JournalDirectoryNodeKind::Unknown;
        assert!(store.register_directory_move("13", ledger.clone()).is_err());
        ledger.children[0].node_kind = JournalDirectoryNodeKind::File;
        ledger.children[0].disposition_recorded = true;
        assert!(store.register_directory_move("13", ledger).is_err());
        assert!(store.document.attempts[0].directory_moves.is_empty());
    }

    #[test]
    fn legacy_unknown_node_kind_cannot_be_receipted_or_completed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        store
            .upsert(move_attempt(18, JournalState::Running))
            .unwrap();
        store
            .register_directory_move("18", directory_ledger())
            .unwrap();
        store
            .record_directory_move_intent("18", 0, child_intent())
            .unwrap();

        let mut legacy = serde_json::to_value(&store.document).unwrap();
        let directory_move = &mut legacy["attempts"][0]["directory_moves"][0];
        directory_move["children"][0]
            .as_object_mut()
            .unwrap()
            .remove("node_kind");
        directory_move["pending"]
            .as_object_mut()
            .unwrap()
            .remove("node_kind");
        store
            .file
            .replace(&serde_json::to_vec(&legacy).unwrap())
            .unwrap();
        drop(store);

        let mut recovered = JournalStore::open(path).unwrap();
        let record = &recovered.document.attempts[0];
        assert_eq!(
            record.directory_moves[0].children[0].node_kind,
            JournalDirectoryNodeKind::Unknown
        );
        assert!(matches!(
            recovered.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(_)]
        ));
        let mut legacy_intent = child_intent();
        legacy_intent.node_kind = JournalDirectoryNodeKind::Unknown;
        assert!(
            recovered
                .record_directory_move_receipt("18", 0, legacy_intent)
                .is_err()
        );
        assert!(recovered.recoveries().len() == 1);

        let ledger = &mut recovered.document.attempts[0].directory_moves[0];
        ledger.pending = None;
        ledger.children[0].disposition_recorded = true;
        recovered.flush().unwrap();
        assert!(recovered.complete_directory_move("18", 0).is_err());
        assert!(matches!(
            recovered.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(_)]
        ));
    }

    #[test]
    fn directory_move_attempt_respects_its_serialized_limit() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(move_attempt(4, JournalState::Running))
            .unwrap();
        let mut ledger = directory_ledger();
        ledger.children[0].relative_components = vec![vec![65; MAX_DIRECTORY_ATTEMPT_BYTES]];
        assert!(store.register_directory_move("4", ledger).is_err());
    }

    #[test]
    fn directory_move_root_intent_cannot_grow_attempt_past_serialized_limit() {
        let root = tempfile::tempdir().unwrap();
        let journal_path = root.path().join("journal.json");
        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        store
            .upsert(move_attempt(24, JournalState::Running))
            .unwrap();

        let identity = "i".repeat(800_000);
        let mut ledger = directory_ledger();
        let root_record = ledger.root.as_mut().unwrap();
        root_record.source_identity = identity.clone();
        root_record.destination_identity = identity;
        let intent = JournalDirectoryRootIntent {
            source_identity: root_record.source_identity.clone(),
            destination_identity: root_record.destination_identity.clone(),
        };
        store.register_directory_move("24", ledger).unwrap();

        let registered_size = serde_json::to_vec(&store.document.attempts[0])
            .unwrap()
            .len();
        assert!(registered_size <= MAX_DIRECTORY_ATTEMPT_BYTES);
        let before_document = store.document.clone();
        let before_disk = std::fs::read(&journal_path).unwrap();

        assert!(
            store
                .record_directory_move_root_intent("24", 0, intent)
                .is_err()
        );
        assert_eq!(store.document, before_document);
        assert_eq!(std::fs::read(&journal_path).unwrap(), before_disk);
    }

    #[test]
    fn directory_move_child_intent_cannot_grow_attempt_past_serialized_limit() {
        let root = tempfile::tempdir().unwrap();
        let journal_path = root.path().join("journal.json");
        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        store
            .upsert(move_attempt(25, JournalState::Running))
            .unwrap();

        let identity = "i".repeat(800_000);
        let mut ledger = directory_ledger();
        ledger.children[0].source_identity = identity.clone();
        ledger.children[0].destination_identity = identity;
        ledger.children[0].content_fact.clear();
        let intent = JournalDirectoryChildIntent {
            ordinal: 0,
            node_kind: ledger.children[0].node_kind,
            relative_components: ledger.children[0].relative_components.clone(),
            source_identity: ledger.children[0].source_identity.clone(),
            destination_identity: ledger.children[0].destination_identity.clone(),
            content_fact: ledger.children[0].content_fact.clone(),
        };
        store.register_directory_move("25", ledger).unwrap();

        let registered_size = serde_json::to_vec(&store.document.attempts[0])
            .unwrap()
            .len();
        assert!(registered_size <= MAX_DIRECTORY_ATTEMPT_BYTES);
        let before_document = store.document.clone();
        let before_disk = std::fs::read(&journal_path).unwrap();

        assert!(store.record_directory_move_intent("25", 0, intent).is_err());
        assert_eq!(store.document, before_document);
        assert_eq!(std::fs::read(&journal_path).unwrap(), before_disk);
    }

    #[test]
    fn schema_one_singular_directory_move_migrates_to_schema_five_vector() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let current = JournalStore::open(path.clone()).unwrap();
        let mut old_attempt =
            serde_json::to_value(move_attempt(41, JournalState::Running)).unwrap();
        old_attempt
            .as_object_mut()
            .unwrap()
            .remove("directory_moves");
        let mut old_ledger = directory_ledger();
        old_ledger.pending = Some(child_intent());
        old_attempt.as_object_mut().unwrap().insert(
            "directory_move".into(),
            serde_json::to_value(old_ledger.clone()).unwrap(),
        );
        let old_document = serde_json::json!({"schema": 1, "attempts": [old_attempt]});
        let old_bytes = serde_json::to_vec(&old_document).unwrap();
        assert!(String::from_utf8_lossy(&old_bytes).contains("\"directory_move\":"));
        current.file.replace(&old_bytes).unwrap();
        drop(current);

        let migrated = JournalStore::open(path.clone()).unwrap();
        assert_eq!(migrated.document.schema, SCHEMA);
        assert_eq!(
            migrated.document.attempts[0].directory_moves,
            vec![old_ledger.clone()]
        );
        assert!(matches!(
            migrated.recoveries().as_slice(),
            [RecoveryItem::RecoveryRequired(record)]
                if record.directory_moves[0].pending == Some(child_intent())
        ));
        migrated.flush().unwrap();
        drop(migrated);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["schema"], SCHEMA);
        assert!(persisted["attempts"][0].get("directory_move").is_none());
        assert_eq!(
            persisted["attempts"][0]["directory_moves"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn directory_move_vector_keeps_multiple_items_sorted_and_independent_after_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let source1 = JournalPath {
            units: vec![68, 58, 92, 115, 111, 117, 114, 99, 101],
        };
        let mut attempt = move_attempt(42, JournalState::Running);
        attempt.sources.push(source1.clone());
        let mut store = JournalStore::open(path.clone()).unwrap();
        store.upsert(attempt).unwrap();
        let mut ledger1 = directory_ledger();
        ledger1.item = 1;
        ledger1.source = source1;
        store.register_directory_move("42", ledger1).unwrap();
        store
            .register_directory_move("42", directory_ledger())
            .unwrap();
        assert_eq!(
            store.document.attempts[0]
                .directory_moves
                .iter()
                .map(|ledger| ledger.item)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        store
            .record_directory_move_intent("42", 1, child_intent())
            .unwrap();
        assert!(
            store.document.attempts[0].directory_moves[0]
                .pending
                .is_none()
        );
        assert_eq!(
            store.document.attempts[0].directory_moves[1].pending,
            Some(child_intent())
        );
        store
            .record_directory_move_receipt("42", 1, child_intent())
            .unwrap();
        drop(store);

        let reopened = JournalStore::open(path).unwrap();
        assert_eq!(reopened.document.attempts[0].directory_moves.len(), 2);
        assert!(!reopened.document.attempts[0].directory_moves[0].children[0].disposition_recorded);
        assert!(reopened.document.attempts[0].directory_moves[1].children[0].disposition_recorded);
    }

    #[test]
    fn directory_move_vector_size_limit_covers_all_registered_items() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let source1 = JournalPath {
            units: vec![68, 58, 92, 115, 111, 117, 114, 99, 101],
        };
        let mut attempt = move_attempt(43, JournalState::Running);
        attempt.sources.push(source1.clone());
        let mut store = JournalStore::open(path.clone()).unwrap();
        store.upsert(attempt).unwrap();
        let mut ledger0 = directory_ledger();
        ledger0.children[0].source_identity = "a".repeat(800_000);
        ledger0.children[0].destination_identity = "b".repeat(800_000);
        store.register_directory_move("43", ledger0).unwrap();
        let mut ledger1 = directory_ledger();
        ledger1.item = 1;
        ledger1.source = source1;
        ledger1.children[0].source_identity = "c".repeat(800_000);
        ledger1.children[0].destination_identity = "d".repeat(800_000);
        let before_document = store.document.clone();
        let before_disk = std::fs::read(&path).unwrap();
        assert!(store.register_directory_move("43", ledger1).is_err());
        assert_eq!(store.document, before_document);
        assert_eq!(std::fs::read(path).unwrap(), before_disk);
    }

    #[test]
    fn directory_move_decoder_rejects_unknown_and_schema_three_singular_records() {
        let mut value = serde_json::json!({"schema": 999, "attempts": []});
        assert!(decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value = serde_json::json!({"schema": 3, "attempts": [{"directory_move": null}]});
        assert!(decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn open_preserves_corrupt_and_future_journal_bytes() {
        let root = tempfile::tempdir().unwrap();
        let cases = [
            ("truncated.json", br#"{"schema":3,"attempts":["#.as_slice()),
            (
                "future-schema.json",
                br#"{"schema":999,"attempts":[]}"#.as_slice(),
            ),
        ];

        for (name, bytes) in cases {
            let path = root.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let error = match JournalStore::open(path.clone()) {
                Ok(_) => panic!("invalid journal unexpectedly opened: {name}"),
                Err(error) => error,
            };
            assert!(!error.is_empty(), "journal error must remain diagnosable");
            assert_eq!(
                std::fs::read(path).unwrap(),
                bytes,
                "failed startup must preserve {name} byte-for-byte"
            );
        }
    }

    #[test]
    fn open_rejects_duplicate_attempt_keys_and_preserves_journal_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("duplicate-attempt-key.json");
        let first = attempt(1, JournalState::Running);
        let mut second = attempt(2, JournalState::Terminal);
        second.key = first.key.clone();
        second.kind = JournalKind::Copy;
        let bytes = serde_json::to_vec(&JournalDocument {
            schema: SCHEMA,
            attempts: vec![first, second],
        })
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let error = match JournalStore::open(path.clone()) {
            Ok(_) => panic!("journal with duplicate attempt keys unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            error.contains("duplicate attempt key"),
            "duplicate-key diagnostic should identify the malformed identity: {error}"
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    fn cross_volume_replace_ledger(
        phase: CrossVolumeMoveReplacePhase,
    ) -> JournalCrossVolumeMoveReplace {
        let (stage_identity, content_sha256) = match phase {
            CrossVolumeMoveReplacePhase::StageCreateIntent | CrossVolumeMoveReplacePhase::StageNotCreated => (String::new(), String::new()),
            CrossVolumeMoveReplacePhase::StageOwned => ("00000002:0000000000000002".into(), String::new()),
            CrossVolumeMoveReplacePhase::StageDiscarded => ("00000002:0000000000000002".into(), String::new()),
            _ => ("00000002:0000000000000002".into(), "a".repeat(64)),
        };
        JournalCrossVolumeMoveReplace {
            item: 0,
            source: JournalPath::from_path(Path::new(r"C:\source.txt")),
            stage: JournalPath::from_path(Path::new(r"D:\.stage.txt")),
            destination: JournalPath::from_path(Path::new(r"D:\target.txt")),
            backup: JournalPath::from_path(Path::new(r"D:\.backup.txt")),
            source_identity: "00000001:0000000000000001".into(),
            stage_identity,
            approved_target_identity: "00000002:0000000000000003".into(),
            destination_parent_identity: "00000002:0000000000000004".into(),
            content_sha256,
            phase,
        }
    }

    fn cross_volume_replace_attempt() -> JournalAttempt {
        let mut record = attempt(91, JournalState::Running);
        record.kind = JournalKind::Move;
        record.sources = vec![JournalPath::from_path(Path::new(r"C:\source.txt"))];
        let ledger = cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady);
        record.destination = Some(ledger.destination.clone());
        record.replace_backup = Some(JournalReplaceBackup {
            source: ledger.source.clone(),
            destination: ledger.destination.clone(),
            backup: ledger.backup.clone(),
        });
        record
    }

    #[test]
    fn cross_volume_replace_requires_single_item_and_exact_adjacent_phase_receipts() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        let mut record = cross_volume_replace_attempt();
        store.upsert(record.clone()).unwrap();
        let mut ledger = cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady);
        store
            .register_cross_volume_move_replace("91", ledger.clone())
            .unwrap();
        assert!(
            store
                .register_cross_volume_move_replace("91", ledger.clone())
                .is_ok()
        );

        let mut stale = ledger.clone();
        stale.stage_identity = "00000002:0000000000000099".into();
        assert!(
            store
                .advance_cross_volume_move_replace("91", stale)
                .is_err()
        );
        let mut skipped = ledger.clone();
        skipped.phase = CrossVolumeMoveReplacePhase::TargetDisplaced;
        assert!(
            store
                .advance_cross_volume_move_replace("91", skipped)
                .is_err()
        );

        for phase in [
            CrossVolumeMoveReplacePhase::DisplaceIntent,
            CrossVolumeMoveReplacePhase::TargetDisplaced,
            CrossVolumeMoveReplacePhase::PublishIntent,
            CrossVolumeMoveReplacePhase::DestinationPublished,
            CrossVolumeMoveReplacePhase::SourceDeleteIntent,
            CrossVolumeMoveReplacePhase::SourceDeleted,
            CrossVolumeMoveReplacePhase::BackupDeleteIntent,
            CrossVolumeMoveReplacePhase::BackupDeleted,
            CrossVolumeMoveReplacePhase::Complete,
        ] {
            ledger.phase = phase;
            store
                .advance_cross_volume_move_replace("91", ledger.clone())
                .unwrap();
            assert!(
                store
                    .advance_cross_volume_move_replace("91", ledger.clone())
                    .is_ok()
            );
        }
        record.cross_volume_move_replace = Some(ledger.clone());
        record.state = JournalState::Terminal;
        record.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        store.upsert(record).unwrap();
        assert!(store.recoveries().is_empty());

        let mut multiple = cross_volume_replace_attempt();
        multiple.key = "92".into();
        multiple
            .sources
            .push(JournalPath::from_path(Path::new(r"C:\second.txt")));
        store.upsert(multiple).unwrap();
        assert!(
            store
                .register_cross_volume_move_replace(
                    "92",
                    cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady)
                )
                .is_err()
        );
    }

    #[test]
    fn cross_volume_replace_stage_create_registration_is_atomic_and_phase_facts_are_monotonic() {
        use CrossVolumeMoveReplacePhase as Phase;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut attempt = cross_volume_replace_attempt();
        attempt.replace_backup = None;
        store.upsert(attempt.clone()).unwrap();

        let intent = cross_volume_replace_ledger(Phase::StageCreateIntent);
        store.register_cross_volume_move_replace("91", intent.clone()).unwrap();
        assert_eq!(store.document.attempts[0].replace_backup.as_ref().unwrap().backup, intent.backup);
        assert_eq!(store.document.attempts[0].cross_volume_move_replace, Some(intent.clone()));
        store.register_cross_volume_move_replace("91", intent.clone()).unwrap();
        let mut skipped = intent.clone();
        skipped.phase = Phase::StageReady;
        skipped.stage_identity = "00000002:0000000000000002".into();
        skipped.content_sha256 = "a".repeat(64);
        assert!(store.advance_cross_volume_move_replace("91", skipped).is_err());

        let mut owned = intent.clone();
        owned.phase = Phase::StageOwned;
        owned.stage_identity = "00000002:0000000000000002".into();
        store.advance_cross_volume_move_replace("91", owned.clone()).unwrap();
        let mut ready = owned.clone();
        ready.phase = Phase::StageReady;
        ready.content_sha256 = "a".repeat(64);
        store.advance_cross_volume_move_replace("91", ready.clone()).unwrap();
        store.advance_cross_volume_move_replace("91", ready.clone()).unwrap();

        let mut changed = ready.clone();
        changed.stage_identity = "00000002:0000000000000099".into();
        assert!(store.advance_cross_volume_move_replace("91", changed).is_err());

        let legacy = serde_json::to_value(&ready).unwrap();
        let decoded: JournalCrossVolumeMoveReplace = serde_json::from_value(legacy).unwrap();
        assert!(!decoded.stage_identity.is_empty());
        assert_eq!(decoded.content_sha256.len(), 64);

        drop(store);
        let reopened = JournalStore::open(path).unwrap();
        assert_eq!(reopened.document.attempts[0].cross_volume_move_replace, Some(ready));
    }

    #[test]
    fn cross_volume_replace_stage_create_rejects_conflicting_backup_reservation() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        let mut attempt = cross_volume_replace_attempt();
        attempt.replace_backup.as_mut().unwrap().backup = JournalPath::from_path(Path::new(r"D:\other-backup.txt"));
        store.upsert(attempt).unwrap();
        assert!(store.register_cross_volume_move_replace("91", cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageCreateIntent)).is_err());
        assert!(store.document.attempts[0].cross_volume_move_replace.is_none());
    }

    #[test]
    fn cross_volume_replace_closed_prepublication_failure_projects_as_terminal() {
        use CrossVolumeMoveReplacePhase as Phase;
        for terminal_phase in [Phase::StageNotCreated, Phase::StageDiscarded] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("journal.json");
            let mut store = JournalStore::open(path.clone()).unwrap();
            let mut attempt = cross_volume_replace_attempt();
            attempt.replace_backup = None;
            store.upsert(attempt.clone()).unwrap();
            let mut ledger = cross_volume_replace_ledger(Phase::StageCreateIntent);
            store.register_cross_volume_move_replace("91", ledger.clone()).unwrap();
            if terminal_phase == Phase::StageDiscarded {
                ledger.phase = Phase::StageOwned;
                ledger.stage_identity = "00000002:0000000000000002".into();
                store.advance_cross_volume_move_replace("91", ledger.clone()).unwrap();
            }
            ledger = cross_volume_replace_ledger(terminal_phase);
            store.advance_cross_volume_move_replace("91", ledger.clone()).unwrap();
            attempt.cross_volume_move_replace = Some(ledger);
            attempt.replace_backup = store.document.attempts[0].replace_backup.clone();
            attempt.state = JournalState::Terminal;
            attempt.terminal_outcome = Some(JournalTerminalOutcome::Failed);
            store.upsert(attempt).unwrap();
            drop(store);
            assert!(JournalStore::open(path).unwrap().recoveries().is_empty());
        }
    }

    #[test]
    fn cross_volume_replace_noncomplete_ledger_overrides_stale_success_terminal() {
        let phases = [
            CrossVolumeMoveReplacePhase::StageReady,
            CrossVolumeMoveReplacePhase::DisplaceIntent,
            CrossVolumeMoveReplacePhase::TargetDisplaced,
            CrossVolumeMoveReplacePhase::PublishIntent,
            CrossVolumeMoveReplacePhase::DestinationPublished,
            CrossVolumeMoveReplacePhase::SourceDeleteIntent,
            CrossVolumeMoveReplacePhase::SourceDeleted,
            CrossVolumeMoveReplacePhase::BackupDeleteIntent,
            CrossVolumeMoveReplacePhase::BackupDeleted,
        ];

        for (case, phase) in phases.into_iter().enumerate() {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("journal.json");
            let mut store = JournalStore::open(path.clone()).unwrap();
            let mut record = cross_volume_replace_attempt();
            store.upsert(record.clone()).unwrap();

            let mut ledger = cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady);
            store
                .register_cross_volume_move_replace("91", ledger.clone())
                .unwrap();
            for next_phase in phases.iter().copied().skip(1).take(case) {
                ledger.phase = next_phase;
                store
                    .advance_cross_volume_move_replace("91", ledger.clone())
                    .unwrap();
            }
            assert_eq!(ledger.phase, phase);

            record.cross_volume_move_replace = Some(ledger);
            record.state = JournalState::Terminal;
            record.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
            store.upsert(record.clone()).unwrap();
            drop(store);

            let reopened = JournalStore::open(path).unwrap();
            let recoveries = reopened.recoveries();
            assert!(matches!(
                recoveries.as_slice(),
                [RecoveryItem::RecoveryRequired(_)]
            ));
            let RecoveryItem::RecoveryRequired(recovered) = &recoveries[0] else {
                unreachable!();
            };
            let mut expected = record;
            expected.state = JournalState::RecoveryRequired;
            assert_eq!(recovered, &expected, "phase {phase:?}");
        }
    }

    #[test]
    fn cross_volume_replace_stage_discarded_is_a_terminal_receipt_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut attempt = cross_volume_replace_attempt();
        store.upsert(attempt.clone()).unwrap();
        let mut ledger = cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady);
        store
            .register_cross_volume_move_replace("91", ledger.clone())
            .unwrap();
        ledger.phase = CrossVolumeMoveReplacePhase::DisplaceIntent;
        store
            .advance_cross_volume_move_replace("91", ledger.clone())
            .unwrap();
        ledger.phase = CrossVolumeMoveReplacePhase::StageDiscarded;
        store
            .advance_cross_volume_move_replace("91", ledger.clone())
            .unwrap();
        let mut forbidden = ledger.clone();
        forbidden.phase = CrossVolumeMoveReplacePhase::DisplaceIntent;
        assert!(
            store
                .advance_cross_volume_move_replace("91", forbidden)
                .is_err()
        );
        attempt.cross_volume_move_replace = Some(ledger);
        attempt.state = JournalState::Terminal;
        attempt.terminal_outcome = Some(JournalTerminalOutcome::Cancelled);
        store.upsert(attempt).unwrap();
        assert!(store.recoveries().is_empty());
        drop(store);
        let reopened = JournalStore::open(path).unwrap();
        assert!(reopened.recoveries().is_empty());
        let encoded = std::fs::read_to_string(root.path().join("journal.json")).unwrap();
        assert!(encoded.contains("\"phase\":\"stage-discarded\""));
    }

    #[test]
    fn cross_volume_replace_recovery_required_rejects_phase_advances_without_mutating_bytes() {
        use CrossVolumeMoveReplacePhase as Phase;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.json");
        let mut store = JournalStore::open(path.clone()).unwrap();
        let mut attempt = cross_volume_replace_attempt();
        store.upsert(attempt.clone()).unwrap();

        let intent = cross_volume_replace_ledger(Phase::StageCreateIntent);
        store
            .register_cross_volume_move_replace("91", intent.clone())
            .unwrap();
        let mut owned = intent;
        owned.phase = Phase::StageOwned;
        owned.stage_identity = "00000002:0000000000000002".into();
        store
            .advance_cross_volume_move_replace("91", owned.clone())
            .unwrap();

        attempt.cross_volume_move_replace = Some(owned.clone());
        attempt.state = JournalState::RecoveryRequired;
        attempt.terminal_outcome = Some(JournalTerminalOutcome::Partial);
        attempt.receipts_recorded = true;
        store.upsert(attempt.clone()).unwrap();
        assert_eq!(store.attempt("91"), Some(&attempt));

        let mut discarded = owned.clone();
        discarded.phase = Phase::StageDiscarded;
        let mut ready = owned.clone();
        ready.phase = Phase::StageReady;
        ready.content_sha256 = "a".repeat(64);
        for (label, candidate) in [
            ("StageOwned to StageDiscarded", discarded),
            ("StageOwned to StageReady", ready),
            ("exact StageOwned duplicate", owned),
        ] {
            let predecessor_bytes = std::fs::read(&path).unwrap();
            assert!(
                store
                    .advance_cross_volume_move_replace("91", candidate)
                    .is_err(),
                "RecoveryRequired accepted {label}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), predecessor_bytes, "{label}");
            assert_eq!(store.attempt("91"), Some(&attempt), "{label}");
        }
    }

    #[test]
    fn cross_volume_replace_cancel_requested_stage_ready_recovery_round_trips() {
        for outcome in [
            JournalTerminalOutcome::Partial,
            JournalTerminalOutcome::Unknown,
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("journal.json");
            let mut store = JournalStore::open(path.clone()).unwrap();
            let mut record = cross_volume_replace_attempt();
            store.upsert(record.clone()).unwrap();
            let ledger = cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady);
            store
                .register_cross_volume_move_replace("91", ledger.clone())
                .unwrap();

            record.cross_volume_move_replace = Some(ledger);
            record.state = JournalState::CancelRequested;
            store.upsert(record.clone()).unwrap();
            record.state = JournalState::RecoveryRequired;
            record.terminal_outcome = Some(outcome);
            record.receipts_recorded = true;
            store.upsert(record.clone()).unwrap();
            assert_eq!(store.attempt("91"), Some(&record));

            drop(store);
            let reopened = JournalStore::open(path).unwrap();
            assert_eq!(reopened.attempt("91"), Some(&record));
            assert!(matches!(
                reopened.recoveries().as_slice(),
                [RecoveryItem::RecoveryRequired(_)]
            ));
        }
    }

    #[test]
    fn cross_volume_replace_cancel_requested_stage_ready_recovery_rejects_invalid_successors() {
        let mutations: [(&str, fn(&mut JournalAttempt)); 10] = [
            ("live state", |candidate| {
                candidate.state = JournalState::Running;
                candidate.terminal_outcome = None;
                candidate.receipts_recorded = false;
            }),
            ("cancelled outcome", |candidate| {
                candidate.terminal_outcome = Some(JournalTerminalOutcome::Cancelled);
            }),
            ("failed outcome", |candidate| {
                candidate.terminal_outcome = Some(JournalTerminalOutcome::Failed);
            }),
            ("succeeded outcome", |candidate| {
                candidate.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
            }),
            ("changed ledger", |candidate| {
                candidate
                    .cross_volume_move_replace
                    .as_mut()
                    .unwrap()
                    .stage_identity = "00000002:0000000000000099".into();
            }),
            ("malformed ledger", |candidate| {
                candidate
                    .cross_volume_move_replace
                    .as_mut()
                    .unwrap()
                    .content_sha256 = "not-a-hash".into();
            }),
            ("omitted ledger", |candidate| {
                candidate.cross_volume_move_replace = None;
            }),
            ("multiple sources", |candidate| {
                candidate.sources.push(candidate.sources[0].clone());
            }),
            ("non-Move kind", |candidate| candidate.kind = JournalKind::Copy),
            ("changed unrelated field", |candidate| candidate.destination = None),
        ];

        for (label, mutate) in mutations {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("journal.json");
            let mut store = JournalStore::open(path.clone()).unwrap();
            let mut record = cross_volume_replace_attempt();
            store.upsert(record.clone()).unwrap();
            let ledger = cross_volume_replace_ledger(CrossVolumeMoveReplacePhase::StageReady);
            store
                .register_cross_volume_move_replace("91", ledger.clone())
                .unwrap();
            record.cross_volume_move_replace = Some(ledger);
            record.state = JournalState::CancelRequested;
            store.upsert(record.clone()).unwrap();

            let mut candidate = record;
            candidate.state = JournalState::RecoveryRequired;
            candidate.terminal_outcome = Some(JournalTerminalOutcome::Partial);
            candidate.receipts_recorded = true;
            mutate(&mut candidate);
            let predecessor_bytes = std::fs::read(&path).unwrap();
            assert!(store.upsert(candidate).is_err(), "accepted {label}");
            assert_eq!(std::fs::read(&path).unwrap(), predecessor_bytes, "{label}");
        }
    }

    #[test]
    fn discard_never_replays_or_mutates_the_recorded_path() {
        let root = tempfile::tempdir().unwrap();
        let mut store = JournalStore::open(root.path().join("journal.json")).unwrap();
        store
            .upsert(attempt(1, JournalState::CancelRequested))
            .unwrap();
        assert!(store.discard("1").unwrap());
        assert!(store.recoveries().is_empty());
    }

    fn directory_move_replace_rename_ledger(
        phase: DirectoryMoveReplaceRenamePhase,
    ) -> JournalDirectoryMoveReplaceRename {
        use DirectoryMoveReplaceRenamePhase as Phase;
        use DirectoryMoveReplaceSourceLocation as Source;
        use DirectoryMoveReplaceTargetLocation as Target;
        let root = std::env::temp_dir().join("gpui-filer-journal-rename-replace");
        let source_parent = root.join("source-parent");
        let destination_parent = root.join("destination-parent");
        let source = source_parent.join("incoming");
        let destination = destination_parent.join("incoming");
        let backup = destination_parent.join(".incoming-backup-unique");
        let approved_target_children = vec![
            JournalDirectoryMoveReplaceRenameChild {
                ordinal: 0,
                node_kind: JournalDirectoryNodeKind::File,
                relative_components: vec!["nested".encode_utf16().collect(), "a.txt".encode_utf16().collect()],
                identity: "child-0".into(),
                content_fact: "file-fact-0".into(),
                disposition_recorded: matches!(phase, Phase::BackupComplete | Phase::Complete),
            },
            JournalDirectoryMoveReplaceRenameChild {
                ordinal: 1,
                node_kind: JournalDirectoryNodeKind::Directory,
                relative_components: vec!["nested".encode_utf16().collect()],
                identity: "child-1".into(),
                content_fact: "directory-fact-1".into(),
                disposition_recorded: matches!(phase, Phase::BackupComplete | Phase::Complete),
            },
        ];
        let (source_location, target_location, backup_root_disposed) = match phase {
            Phase::Prepared | Phase::NoEffect => (Source::Original, Target::Destination, false),
            Phase::DisplaceIntent => (Source::Original, Target::Uncertain, false),
            Phase::TargetDisplaced => (Source::Original, Target::Backup, false),
            Phase::PublishIntent => (Source::Uncertain, Target::Backup, false),
            Phase::DestinationPublished | Phase::BackupCleanup => (Source::Published, Target::Backup, false),
            Phase::BackupComplete | Phase::Complete => (Source::Published, Target::Disposed, true),
        };
        JournalDirectoryMoveReplaceRename {
            item: 0,
            source: JournalPath::from_path(&source),
            destination: JournalPath::from_path(&destination),
            backup: JournalPath::from_path(&backup),
            source_identity: "source-root".into(),
            approved_target_identity: "target-root".into(),
            volume_identity: "volume-identity".into(),
            source_parent: JournalPath::from_path(&source_parent),
            source_parent_identity: "source-parent".into(),
            destination_parent: JournalPath::from_path(&destination_parent),
            destination_parent_identity: "destination-parent".into(),
            phase,
            source_location,
            target_location,
            approved_target_children,
            pending_cleanup: None,
            backup_root_disposed,
        }
    }

    fn attempt_with_directory_move_replace_rename(
        phase: DirectoryMoveReplaceRenamePhase,
    ) -> JournalAttempt {
        let ledger = directory_move_replace_rename_ledger(phase);
        let mut record = attempt(77, JournalState::RecoveryRequired);
        record.kind = JournalKind::Move;
        record.sources = vec![ledger.source.clone()];
        record.destination = Some(ledger.destination_parent.clone());
        record.directory_move_replace_rename = Some(ledger);
        record
    }

    #[test]
    fn directory_move_replace_rename_accepts_exact_monotonic_prefix_and_recovery_locations() {
        use DirectoryMoveReplaceRenamePhase as Phase;
        for phase in [
            Phase::Prepared,
            Phase::NoEffect,
            Phase::DisplaceIntent,
            Phase::TargetDisplaced,
            Phase::PublishIntent,
            Phase::DestinationPublished,
            Phase::BackupCleanup,
            Phase::BackupComplete,
            Phase::Complete,
        ] {
            let record = attempt_with_directory_move_replace_rename(phase);
            validate_directory_move_vector(&record).unwrap_or_else(|error| panic!("{phase:?}: {error}"));
        }

        for (from, to) in [
            (Phase::Prepared, Phase::NoEffect),
            (Phase::Prepared, Phase::DisplaceIntent),
            (Phase::DisplaceIntent, Phase::TargetDisplaced),
            (Phase::TargetDisplaced, Phase::PublishIntent),
            (Phase::PublishIntent, Phase::DestinationPublished),
            (Phase::DestinationPublished, Phase::BackupCleanup),
            (Phase::BackupCleanup, Phase::BackupComplete),
            (Phase::BackupComplete, Phase::Complete),
        ] {
            assert!(directory_move_replace_rename_phase_next(from, to));
        }
        assert!(!directory_move_replace_rename_phase_next(Phase::Prepared, Phase::TargetDisplaced));
        assert!(!directory_move_replace_rename_phase_next(Phase::PublishIntent, Phase::Complete));
        assert!(!directory_move_replace_rename_phase_next(Phase::Complete, Phase::Complete));

        let mut partial_cleanup = attempt_with_directory_move_replace_rename(Phase::BackupCleanup);
        let children = &mut partial_cleanup
            .directory_move_replace_rename
            .as_mut()
            .unwrap()
            .approved_target_children;
        children[0].disposition_recorded = true;
        validate_directory_move_vector(&partial_cleanup).unwrap();
    }

    #[test]
    fn directory_move_replace_rename_rejects_wrong_kind_and_malformed_routes_or_identities() {
        use DirectoryMoveReplaceRenamePhase as Phase;
        let mut record = attempt_with_directory_move_replace_rename(Phase::Prepared);
        record.kind = JournalKind::Copy;
        assert!(validate_directory_move_vector(&record).is_err());

        let mut record = attempt_with_directory_move_replace_rename(Phase::Prepared);
        let destination = record.directory_move_replace_rename.as_ref().unwrap().destination.clone();
        record.directory_move_replace_rename.as_mut().unwrap().backup = destination;
        assert!(validate_directory_move_vector(&record).is_err());

        let mut record = attempt_with_directory_move_replace_rename(Phase::Prepared);
        record.directory_move_replace_rename.as_mut().unwrap().source_identity.clear();
        assert!(validate_directory_move_vector(&record).is_err());

        let mut record = attempt_with_directory_move_replace_rename(Phase::Prepared);
        record.directory_move_replace_rename.as_mut().unwrap().volume_identity.clear();
        assert!(validate_directory_move_vector(&record).is_err());

        let mut record = attempt_with_directory_move_replace_rename(Phase::Prepared);
        record.directory_move_replace_rename.as_mut().unwrap().source_location =
            DirectoryMoveReplaceSourceLocation::Uncertain;
        assert!(validate_directory_move_vector(&record).is_err());

        let mut record = attempt_with_directory_move_replace_rename(Phase::DisplaceIntent);
        record
            .directory_move_replace_rename
            .as_mut()
            .unwrap()
            .approved_target_children[0]
            .disposition_recorded = true;
        assert!(validate_directory_move_vector(&record).is_err());

        let mut record = attempt_with_directory_move_replace_rename(Phase::BackupCleanup);
        let ledger = record.directory_move_replace_rename.as_mut().unwrap();
        ledger.approved_target_children[0].disposition_recorded = true;
        ledger.backup_root_disposed = true;
        assert!(validate_directory_move_vector(&record).is_err());
    }

    #[test]
    fn directory_move_replace_rename_legacy_attempt_defaults_to_no_ledger() {
        let mut value = serde_json::to_value(attempt(78, JournalState::RecoveryRequired)).unwrap();
        value.as_object_mut().unwrap().remove("directory_move_replace_rename");
        let decoded: JournalAttempt = serde_json::from_value(value).unwrap();
        assert!(decoded.directory_move_replace_rename.is_none());
    }
}
