use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

/// Process-lifetime identity. It is allocated only by [`OperationService`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);

/// One execution of a job. A retry always receives a new value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttemptId(u64);

/// Every executor callback must carry this complete identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttemptToken {
    job: JobId,
    attempt: AttemptId,
}

/// Historical directory Move evidence projected from the durable journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryMoveReview {
    pub attempt: AttemptToken,
    pub journal_key: String,
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub children_total: usize,
    pub children_dispositions_recorded: usize,
    pub pending_child: bool,
    pub root_disposition_recorded: bool,
    pub pending_root: bool,
    pub cleanup_complete: bool,
    /// Root identities recorded in the exact durable DirectoryMove ledger.
    pub source_root_identity: Option<String>,
    pub destination_root_identity: Option<String>,
    /// Fresh point-in-time root-only observations; they say nothing about children.
    pub source_root_observation: CopyInspectionState,
    pub destination_root_observation: CopyInspectionState,
    pub replace: Option<DirectoryMoveReplaceReview>,
    pub stage_root_observation: CopyInspectionState,
    pub backup_root_observation: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryMoveReplaceReview {
    pub stage: std::path::PathBuf,
    pub backup: std::path::PathBuf,
    pub phase: super::journal::DirectoryMoveReplacePhase,
    pub backup_root_disposition_recorded: bool,
    pub backup_cleanup_complete: bool,
    pub backup_children_total: usize,
    pub backup_children_dispositions_recorded: usize,
    pub pending_backup_intent: bool,
    pub stage_identity: Option<String>,
    pub displaced_identity: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryMoveRootProbe {
    pub attempt: AttemptToken,
    pub journal_key: String,
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub expected_source_identity: Option<String>,
    pub expected_destination_identity: Option<String>,
    pub stage: Option<std::path::PathBuf>,
    pub backup: Option<std::path::PathBuf>,
    pub expected_stage_identity: Option<String>,
    pub expected_backup_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryMoveRootObservations {
    pub source: CopyInspectionState,
    pub destination: CopyInspectionState,
    pub stage: CopyInspectionState,
    pub backup: CopyInspectionState,
}

/// Passive root-only diagnosis for one reopened durable recovery record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReopenedDirectoryMoveDiagnosis {
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub source_root_identity: Option<String>,
    pub destination_root_identity: Option<String>,
    pub source_root_observation: CopyInspectionState,
    pub destination_root_observation: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReopenedDirectoryMoveTicket {
    pub key: String,
    pub owner_incarnation: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReopenedDirectoryMoveProbe {
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub expected_source_identity: Option<String>,
    pub expected_destination_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReopenedDirectoryMoveObservations {
    pub source: CopyInspectionState,
    pub destination: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReopenedDirectoryMoveDiagnosisState {
    Pending,
    Available(ReopenedDirectoryMoveDiagnosis),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReopenedCopyStageReview {
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub historical: super::journal::JournalCopyDirectoryStageEvidence,
    pub stage_root_observation: CopyInspectionState,
    pub destination_root_observation: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReopenedCopyStageTicket {
    pub key: String,
    pub owner_incarnation: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReopenedCopyStageProbe {
    pub stage: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub expected_stage_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReopenedCopyStageObservations {
    pub stage: CopyInspectionState,
    pub destination: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReopenedCopyStageReviewState {
    Pending,
    Available(ReopenedCopyStageReview),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryMoveReviewState {
    Pending,
    Available(DirectoryMoveReview),
    Unavailable,
}

impl AttemptToken {
    pub fn job_id(self) -> JobId {
        self.job
    }

    pub fn attempt_id(self) -> AttemptId {
        self.attempt
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self {
            job: JobId(0),
            attempt: AttemptId(0),
        }
    }
}

impl JobId {
    pub fn value(self) -> u64 {
        self.0
    }
}

impl AttemptId {
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Opaque identity for one conflict observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConflictToken {
    attempt: AttemptToken,
    item: ItemId,
    conflict: u64,
    probe_revision: u64,
}

impl ConflictToken {
    pub(crate) const fn attempt_token(self) -> AttemptToken {
        self.attempt
    }

    pub(crate) const fn item_id(self) -> ItemId {
        self.item
    }
}

/// Returns the bounded sibling name used by the worker's Keep Both policy.
/// Runtime uses the same formatter to validate the worker's selected path.
pub(crate) fn keep_both_candidate_path(base: &Path, suffix: u32) -> Option<PathBuf> {
    if !(2..10_000).contains(&suffix) {
        return None;
    }
    let parent = base.parent()?;
    let stem = base.file_stem().unwrap_or_default().to_string_lossy();
    let extension = base.extension().map(|value| value.to_string_lossy());
    let name = match &extension {
        Some(extension) if !extension.is_empty() => format!("{stem} ({suffix}).{extension}"),
        _ => format!("{stem} ({suffix})"),
    };
    Some(parent.join(name))
}

pub(crate) fn is_keep_both_candidate_path(base: &Path, candidate: &Path) -> bool {
    (2..10_000).any(|suffix| keep_both_candidate_path(base, suffix).as_deref() == Some(candidate))
}

/// Directory Keep Both keeps dotted folder names intact (for example,
/// `folder.name (2)`). File candidates continue to use the extension-aware helper.
pub(crate) fn keep_both_directory_candidate_path(base: &Path, suffix: u32) -> Option<PathBuf> {
    if !(2..10_000).contains(&suffix) {
        return None;
    }
    Some(base.parent()?.join(format!("{} ({suffix})", base.file_name()?.to_string_lossy())))
}

pub(crate) fn is_keep_both_directory_candidate_path(base: &Path, candidate: &Path) -> bool {
    (2..10_000).any(|suffix| keep_both_directory_candidate_path(base, suffix).as_deref() == Some(candidate))
}

/// An item is stable within one submitted request/plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Copy,
    Move,
    /// Clipboard source paths become symbolic-link objects in `destination`.
    /// Unlike Copy, the source contents are never read or traversed.
    CreateSymbolicLink,
    /// Clipboard folder paths become NTFS junction objects in `destination`.
    /// A junction never accepts a file source.
    CreateJunction,
    /// Creates one empty ordinary folder under the captured destination directory.
    CreateFolder,
    /// Creates one empty ordinary file under the captured destination directory.
    CreateFile,
    /// Creates a same-parent Windows ShellLink for one ordinary filesystem object.
    CreateShortcut,
    /// Creates one ZIP containing one captured ordinary filesystem file.
    CreateZip,
    /// Creates one 7z archive from ordered ordinary sibling roots.
    Create7z,
    /// Safely extracts one captured ordinary ZIP into a sibling folder.
    ExtractZip,
    /// Safely extracts ordered captured 7z archives into sibling folders.
    Extract7z,
    Trash,
    Rename,
    BatchRename,
    BatchRenameUndo,
    /// Removes verified source files after an external MOVE publication.
    ExternalMoveCleanup,
    /// Explicit disposition of one process-custodied failed Copy stage.
    CopyStageCleanup,
}

/// Immutable input captured at command acceptance time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashSourceGuard {
    pub identity: crate::directory::EntrySourceIdentity,
    /// The row was observed as a reparse candidate; Windows Trash admits only exact symlink/junction tags.
    pub reparse_candidate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRequest {
    pub kind: OperationKind,
    /// Ordered process-local virtual Shell sources; tokens are passive references and never paths.
    pub virtual_source: Option<super::virtual_transfer::VirtualFileSources>,
    pub sources: Vec<PathBuf>,
    /// No-follow row identities and immutable reparse candidates captured at Trash admission.
    /// Required for every Trash request and kept positionally aligned with `sources`.
    pub trash_source_guards: Option<Vec<TrashSourceGuard>>,
    pub shortcut_create_guard: Option<ShortcutCreateGuard>,
    pub destination: Option<PathBuf>,
    /// Ordered immutable source/destination snapshot for one Batch Rename job.
    /// When present, `sources` must match its source paths in the same order.
    pub batch_rename_plan: Option<Vec<super::batch_rename::RenameTarget>>,
/// Request-time object identities for an identity-bound operation.
/// A fresh Copy from recovery carries aligned source and destination identities
/// plus `initial_conflict_policy` in the same immutable admission snapshot.
/// Ordinary operations leave this unset and retain their existing policy.
    pub move_identity_guard: Option<MoveIdentityGuard>,
    /// CF_HDROP Move source objects observed at Paste capture time, aligned with `sources`.
    pub clipboard_move_source_evidence: Option<Vec<ClipboardMoveSourceEvidence>>,
    /// No-follow acceptance proof for the narrow Windows symbolic-link/junction Rename path.
    pub rename_reparse_guard: Option<RenameReparseGuard>,
    /// Destination-only identity for background Paste and Paste Here Move. It
    /// does not assert source identity by itself; clipboard Move Replace also
    /// requires `move_replace_source_evidence`.
    pub move_destination_identity_guard: Option<crate::filesystem_identity::FilesystemIdentity>,
    /// Request-ordered regular-file Clipboard Move source evidence captured before admission.
    /// When present, cardinality and index correspond exactly to `sources` / `ItemId`.
    pub move_replace_source_evidence: Option<Vec<MoveReplaceSourceEvidence>>,
    /// Request-time identities for source-only external MOVE cleanup.
    pub source_cleanup_identity_guard: Option<SourceCleanupIdentityGuard>,
    /// Destination-only identity for background clipboard Paste and external
    /// Copy dropped on a folder row.
    pub copy_destination_identity_guard: Option<crate::filesystem_identity::FilesystemIdentity>,
    /// Immutable initial conflict policy for operation-owned automatic resolution.
    pub initial_conflict_policy: Option<ConflictDecision>,
    /// Exact destination-directory identity accepted for guarded creates and external symbolic links.
    pub create_parent_directory_identity_guard:
        Option<crate::filesystem_identity::FilesystemIdentity>,
    /// Immutable create or extract identity proof; paths remain authoritative in `sources`.
    pub zip_create_guard: Option<ZipOperationGuard>,
    /// Passive provider and source facts captured for a future 7z worker.
    pub seven_zip_create_guard: Option<SevenZipCreateGuard>,
    /// Passive request-time facts for a future 7z extraction worker.
    pub seven_zip_extract_guard: Option<SevenZipExtractGuard>,
    /// Presentation routing only; this never owns or cancels the job.
    pub origin: Option<OperationOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutCreateGuard {
    pub source_kind: crate::directory::EntryKind,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub parent_identity: crate::filesystem_identity::FilesystemIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveReplaceSourceEvidence {
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardMoveSourceEvidence {
    pub source: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_kind: crate::directory::EntryKind,
    pub reparse_tag: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipCreateGuard {
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_kind: crate::directory::EntryKind,
    /// Unicode ZIP entry name captured from the source leaf at request time.
    pub entry_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenZipCreateSourceGuard {
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_kind: crate::directory::EntryKind,
    pub entry_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenZipCreateGuard {
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub sources: Vec<SevenZipCreateSourceGuard>,
    pub provider_revision: u64,
    pub provider_binding: crate::archive_provider::ArchiveOperationBinding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenZipExtractSourceGuard {
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_kind: crate::directory::EntryKind,
    pub entry_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenZipExtractGuard {
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub sources: Vec<SevenZipExtractSourceGuard>,
    pub provider_revision: u64,
    pub provider_binding: crate::archive_provider::ArchiveOperationBinding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipCreateSourceGuard {
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_kind: crate::directory::EntryKind,
    pub entry_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipArchiveCreateGuard {
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub sources: Vec<ZipCreateSourceGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipArchiveExtractGuard {
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub sources: Vec<ZipCreateSourceGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZipOperationGuard {
    Create(ZipArchiveCreateGuard),
    Extract(ZipCreateGuard),
    ExtractMany(ZipArchiveExtractGuard),
}

impl OperationRequest {
    pub fn create_shortcut(
        source: PathBuf,
        destination_parent: PathBuf,
        guard: ShortcutCreateGuard,
    ) -> Result<Self, String> {
        let request = Self {
            kind: OperationKind::CreateShortcut,
            virtual_source: None,
            sources: vec![source],
            trash_source_guards: None,
            destination: Some(destination_parent),
            batch_rename_plan: None,
            move_identity_guard: None,
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
            source_cleanup_identity_guard: None,
            copy_destination_identity_guard: None,
            initial_conflict_policy: None,
            create_parent_directory_identity_guard: None,
            zip_create_guard: None,
            shortcut_create_guard: Some(guard),
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        };
        request
            .shortcut_create_shape_is_valid()
            .then_some(request)
            .ok_or_else(|| "shortcut creation requires one captured ordinary sibling source".into())
    }

    pub fn shortcut_output_path(&self) -> Option<PathBuf> {
        let source = self.sources.first()?;
        let parent = source.parent()?;
        let mut leaf = std::ffi::OsString::from(source.file_name()?);
        leaf.push(".lnk");
        Some(parent.join(leaf))
    }

    pub(crate) fn shortcut_create_shape_is_valid(&self) -> bool {
        let (Some(source), Some(parent), Some(guard)) = (
            (self.sources.len() == 1).then(|| self.sources.first()).flatten(),
            self.destination.as_ref(),
            self.shortcut_create_guard.as_ref(),
        ) else {
            return false;
        };
        matches!(guard.source_kind, crate::directory::EntryKind::File | crate::directory::EntryKind::Folder)
            && guard.source_identity.windows_parts().is_some()
            && guard.parent_identity.windows_parts().is_some()
            && source.file_name().is_some()
            && source.parent() == Some(parent.as_path())
            && self.virtual_source.is_none()
            && self.trash_source_guards.is_none()
            && self.batch_rename_plan.is_none()
            && self.move_identity_guard.is_none()
            && self.clipboard_move_source_evidence.is_none()
            && self.rename_reparse_guard.is_none()
            && self.move_destination_identity_guard.is_none()
            && self.move_replace_source_evidence.is_none()
            && self.source_cleanup_identity_guard.is_none()
            && self.copy_destination_identity_guard.is_none()
            && self.initial_conflict_policy.is_none()
            && self.create_parent_directory_identity_guard.is_none()
            && self.zip_create_guard.is_none()
            && self.seven_zip_create_guard.is_none()
            && self.seven_zip_extract_guard.is_none()
    }

    /// Build a ZIP request from identity facts captured by the command owner.
    /// This constructor performs no filesystem I/O, so it is safe on the input path.
    pub fn create_zip(
        source: PathBuf,
        destination: PathBuf,
        guard: ZipCreateGuard,
    ) -> Result<Self, String> {
        let source_guard = ZipCreateSourceGuard {
            source_identity: guard.source_identity,
            source_kind: guard.source_kind,
            entry_name: guard.entry_name,
        };
        Self::create_zip_many(
            vec![source],
            destination,
            ZipArchiveCreateGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![source_guard],
            },
        )
    }

    pub fn create_zip_many(
        sources: Vec<PathBuf>,
        destination: PathBuf,
        guard: ZipArchiveCreateGuard,
    ) -> Result<Self, String> {
        validate_zip_create_facts(&sources, &destination, &guard)?;
        Ok(Self {
            kind: OperationKind::CreateZip,
            virtual_source: None,
            sources,
            destination: Some(destination),
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
            zip_create_guard: Some(ZipOperationGuard::Create(guard)),
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        })
    }

    pub(crate) fn zip_create_shape_is_valid(&self) -> bool {
        let (Some(destination), Some(ZipOperationGuard::Create(guard))) =
            (self.destination.as_ref(), self.zip_create_guard.as_ref())
        else {
            return false;
        };
        validate_zip_create_facts(&self.sources, destination, guard).is_ok()
    }

    pub fn extract_zip(
        source: PathBuf,
        destination: PathBuf,
        guard: ZipCreateGuard,
    ) -> Result<Self, String> {
        if !source
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
            || guard.source_kind != crate::directory::EntryKind::File
            || guard.source_identity.windows_parts().is_none()
            || guard.destination_parent_identity.windows_parts().is_none()
        {
            return Err("ZIP extraction requires one captured ordinary .zip file".into());
        }
        let parent = source
            .parent()
            .ok_or("ZIP source needs an explicit parent")?;
        let expected_parent = destination
            .parent()
            .ok_or("ZIP output needs an explicit parent")?;
        if parent != expected_parent
            || destination
                .file_name()
                .and_then(|name| name.to_str())
                .is_none_or(str::is_empty)
            || destination.file_name() != source.file_stem()
        {
            return Err("ZIP output must be a sibling folder with a Unicode name".into());
        }
        Ok(Self {
            kind: OperationKind::ExtractZip,
            virtual_source: None,
            sources: vec![source],
            destination: Some(destination),
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
            zip_create_guard: Some(ZipOperationGuard::Extract(guard)),
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        })
    }

    pub fn extract_zip_many(
        sources: Vec<PathBuf>,
        guard: ZipArchiveExtractGuard,
    ) -> Result<Self, String> {
        if sources.is_empty() || sources.len() != guard.sources.len() || sources.len() > 1024 {
            return Err("ZIP extraction needs ordered source guards".into());
        }
        let parent = sources[0]
            .parent()
            .ok_or("ZIP source needs an explicit parent")?;
        for (source, source_guard) in sources.iter().zip(&guard.sources) {
            if source.parent() != Some(parent)
                || !source
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
                || source_guard.source_kind != crate::directory::EntryKind::File
                || source_guard.source_identity.windows_parts().is_none()
                || source_guard.entry_name
                    != source
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("")
                || source
                    .file_stem()
                    .is_none_or(|stem| stem.to_str().is_none_or(str::is_empty))
            {
                return Err("ZIP extraction requires sibling ordinary .zip files".into());
            }
        }
        let mut stems = Vec::with_capacity(sources.len());
        for source in &sources {
            let stem = source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or("ZIP output needs a Unicode name")?;
            if stems
                .iter()
                .any(|existing: &String| zip_entry_names_collide(existing, stem))
            {
                return Err("ZIP archives have colliding sibling output names".into());
            }
            stems.push(stem.to_owned());
        }
        if guard.destination_parent_identity.windows_parts().is_none() {
            return Err("ZIP extraction requires a captured parent identity".into());
        }
        let destination = parent.join(
            sources[0]
                .file_stem()
                .ok_or("ZIP output needs a Unicode name")?,
        );
        Ok(Self {
            kind: OperationKind::ExtractZip,
            virtual_source: None,
            sources,
            destination: Some(destination),
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
            zip_create_guard: Some(ZipOperationGuard::ExtractMany(guard)),
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        })
    }

    pub fn extract_7z_many(
        sources: Vec<PathBuf>,
        guard: SevenZipExtractGuard,
    ) -> Result<Self, String> {
        if sources.is_empty() || sources.len() != guard.sources.len() || sources.len() > 1024 {
            return Err("7z extraction needs ordered source guards".into());
        }
        let parent = sources[0]
            .parent()
            .ok_or("7z source needs an explicit parent")?;
        let mut outputs = Vec::<String>::with_capacity(sources.len());
        for (source, source_guard) in sources.iter().zip(&guard.sources) {
            let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let stem = source.file_stem().and_then(|n| n.to_str()).unwrap_or("");
            if source.parent() != Some(parent)
                || !seven_zip_path_is_absolute_and_nul_free(source)
                || !seven_zip_path_is_bounded(source)
                || !source
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("7z"))
                || source_guard.source_kind != crate::directory::EntryKind::File
                || source_guard.source_identity.windows_parts().is_none()
                || source_guard.entry_name != name
                || !seven_zip_windows_component(name)
                || !seven_zip_windows_component(stem)
                || guard
                    .sources
                    .iter()
                    .take(outputs.len())
                    .any(|prior| prior.source_identity == source_guard.source_identity)
                || outputs
                    .iter()
                    .any(|prior| zip_entry_names_collide(prior, stem))
            {
                return Err("7z extraction requires ordered sibling ordinary .7z files".into());
            }
            outputs.push(stem.to_owned());
        }
        if guard.destination_parent_identity.windows_parts().is_none()
            || guard.provider_revision == 0
            || !seven_zip_path_is_absolute_and_nul_free(&guard.provider_binding.executable.path)
            || !seven_zip_path_is_absolute_and_nul_free(&guard.provider_binding.zip_dll.path)
            || !seven_zip_path_is_bounded(&guard.provider_binding.executable.path)
            || !seven_zip_path_is_bounded(&guard.provider_binding.zip_dll.path)
            || guard.provider_binding.executable.identity.volume_serial == 0
            || guard.provider_binding.executable.identity.file_index == 0
            || guard.provider_binding.zip_dll.identity.volume_serial == 0
            || guard.provider_binding.zip_dll.identity.file_index == 0
            || guard.provider_binding.executable.identity == guard.provider_binding.zip_dll.identity
        {
            return Err("7z extraction requires captured parent and provider identities".into());
        }
        let destination = parent.join(sources[0].file_stem().ok_or("7z output needs a name")?);
        Ok(Self {
            kind: OperationKind::Extract7z,
            virtual_source: None,
            sources,
            destination: Some(destination),
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
            seven_zip_extract_guard: Some(guard),
            origin: None,
        })
    }

    pub fn create_7z(
        sources: Vec<PathBuf>,
        destination: PathBuf,
        guard: SevenZipCreateGuard,
    ) -> Result<Self, String> {
        validate_seven_zip_create_facts(&sources, &destination, &guard)?;
        Ok(Self {
            kind: OperationKind::Create7z,
            virtual_source: None,
            sources,
            destination: Some(destination),
            trash_source_guards: None,
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
            seven_zip_create_guard: Some(guard),
            seven_zip_extract_guard: None,
            origin: None,
        })
    }

    pub(crate) fn seven_zip_create_shape_is_valid(&self) -> bool {
        self.kind == OperationKind::Create7z
            && self.virtual_source.is_none()
            && self.zip_create_guard.is_none()
            && self.trash_source_guards.is_none()
            && self.batch_rename_plan.is_none()
            && self.move_identity_guard.is_none()
            && self.rename_reparse_guard.is_none()
            && self.move_destination_identity_guard.is_none()
            && self.source_cleanup_identity_guard.is_none()
            && self.copy_destination_identity_guard.is_none()
            && self.initial_conflict_policy.is_none()
            && self.create_parent_directory_identity_guard.is_none()
            && self.seven_zip_create_guard.as_ref().is_some_and(|guard| {
                self.destination.as_ref().is_some_and(|destination| {
                    validate_seven_zip_create_facts(&self.sources, destination, guard).is_ok()
                })
            })
    }

    pub(crate) fn seven_zip_extract_shape_is_valid(&self) -> bool {
        self.kind == OperationKind::Extract7z
            && self.virtual_source.is_none()
            && self.zip_create_guard.is_none()
            && self.trash_source_guards.is_none()
            && self.batch_rename_plan.is_none()
            && self.move_identity_guard.is_none()
            && self.rename_reparse_guard.is_none()
            && self.move_destination_identity_guard.is_none()
            && self.source_cleanup_identity_guard.is_none()
            && self.copy_destination_identity_guard.is_none()
            && self.initial_conflict_policy.is_none()
            && self.create_parent_directory_identity_guard.is_none()
            && self.seven_zip_create_guard.is_none()
            && self.seven_zip_extract_guard.as_ref().is_some_and(|guard| {
                self.destination.as_ref().is_some_and(|destination| {
                    let expected = self
                        .sources
                        .first()
                        .and_then(|source| Some(source.parent()?.join(source.file_stem()?)));
                    expected.as_ref() == Some(destination)
                        && self.sources.len() == guard.sources.len()
                        && self
                            .sources
                            .iter()
                            .zip(&guard.sources)
                            .all(|(source, fact)| {
                                source.parent() == self.sources.first().and_then(|p| p.parent())
                                    && seven_zip_path_is_absolute_and_nul_free(source)
                                    && seven_zip_path_is_bounded(source)
                                    && source
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .is_some_and(|e| e.eq_ignore_ascii_case("7z"))
                                    && fact.source_kind == crate::directory::EntryKind::File
                                    && fact.entry_name
                                        == source.file_name().and_then(|n| n.to_str()).unwrap_or("")
                                    && fact.source_identity.windows_parts().is_some()
                                    && source
                                        .file_stem()
                                        .and_then(|s| s.to_str())
                                        .is_some_and(|stem| seven_zip_windows_component(stem))
                            })
                        && guard.sources.iter().enumerate().all(|(index, fact)| {
                            !guard
                                .sources
                                .iter()
                                .take(index)
                                .any(|prior| prior.source_identity == fact.source_identity)
                                && !self.sources.iter().take(index).any(|source| {
                                    source
                                        .file_stem()
                                        .and_then(|s| s.to_str())
                                        .zip(
                                            self.sources
                                                .get(index)
                                                .and_then(|s| s.file_stem())
                                                .and_then(|s| s.to_str()),
                                        )
                                        .is_some_and(|(a, b)| zip_entry_names_collide(a, b))
                                })
                        })
                        && guard.destination_parent_identity.windows_parts().is_some()
                        && guard.provider_revision > 0
                        && seven_zip_path_is_absolute_and_nul_free(
                            &guard.provider_binding.executable.path,
                        )
                        && seven_zip_path_is_absolute_and_nul_free(
                            &guard.provider_binding.zip_dll.path,
                        )
                        && seven_zip_path_is_bounded(&guard.provider_binding.executable.path)
                        && seven_zip_path_is_bounded(&guard.provider_binding.zip_dll.path)
                        && guard.provider_binding.executable.identity.volume_serial > 0
                        && guard.provider_binding.executable.identity.file_index > 0
                        && guard.provider_binding.zip_dll.identity.volume_serial > 0
                        && guard.provider_binding.zip_dll.identity.file_index > 0
                        && guard.provider_binding.executable.identity
                            != guard.provider_binding.zip_dll.identity
                })
            })
    }
}

fn validate_seven_zip_create_facts(
    sources: &[PathBuf],
    destination: &Path,
    guard: &SevenZipCreateGuard,
) -> Result<(), String> {
    if sources.is_empty() || sources.len() > 1024 || sources.len() != guard.sources.len() {
        return Err("7z needs 1 to 1024 ordered source guards".into());
    }
    if !seven_zip_path_is_absolute_and_nul_free(destination)
        || sources
            .iter()
            .any(|source| !seven_zip_path_is_absolute_and_nul_free(source))
        || !seven_zip_path_is_absolute_and_nul_free(&guard.provider_binding.executable.path)
        || !seven_zip_path_is_absolute_and_nul_free(&guard.provider_binding.zip_dll.path)
    {
        return Err("7z source, output, and provider paths must be absolute and NUL-free".into());
    }
    if !destination
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("7z"))
    {
        return Err("7z destination must have a .7z extension".into());
    }
    let parent = sources[0]
        .parent()
        .ok_or("7z source needs an explicit parent")?;
    if destination.parent() != Some(parent)
        || guard.destination_parent_identity.windows_parts().is_none()
    {
        return Err("7z sources and output must share the captured ordinary parent".into());
    }
    let mut names: Vec<String> = Vec::new();
    let output_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("7z output name must be Unicode")?;
    if !seven_zip_windows_component(output_name) {
        return Err("7z output filename is not an ordinary Windows name".into());
    }
    for (source, source_guard) in sources.iter().zip(&guard.sources) {
        if source.parent() != Some(parent)
            || source_guard.source_identity.windows_parts().is_none()
            || !matches!(
                source_guard.source_kind,
                crate::directory::EntryKind::File | crate::directory::EntryKind::Folder
            )
            || zip_entry_name(source)? != source_guard.entry_name
            || !seven_zip_windows_component(&source_guard.entry_name)
        {
            return Err("7z source guard does not match an ordinary sibling source".into());
        }
        if source == destination || zip_entry_names_collide(output_name, &source_guard.entry_name) {
            return Err("7z output name collides with a captured source".into());
        }
        if names
            .iter()
            .any(|name| zip_entry_names_collide(name, &source_guard.entry_name))
        {
            return Err("7z source entry names collide".into());
        }
        names.push(source_guard.entry_name.clone());
    }
    Ok(())
}

pub(crate) fn seven_zip_windows_component(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.encode_utf16().count() > 255
        || component.ends_with('.')
        || component.ends_with(' ')
        || component
            .chars()
            .any(|c| c <= '\u{1f}' || "<>:\"|?*/\\".contains(c))
    {
        return false;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    ["COM", "LPT"].iter().all(|prefix| {
        stem.strip_prefix(prefix).is_none_or(|number| {
            !matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn seven_zip_path_is_absolute_and_nul_free(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        !path.as_os_str().encode_wide().any(|unit| unit == 0)
    }
    #[cfg(not(windows))]
    {
        path.to_str().is_some_and(|path| !path.contains('\0'))
    }
}

fn seven_zip_path_is_bounded(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().take(32_768).count() <= 32_767
    }
    #[cfg(not(windows))]
    {
        path.as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .take(32_768)
            .count()
            <= 32_767
    }
}

pub(crate) fn zip_entry_name(source: &Path) -> Result<&str, String> {
    let source_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "source filename is not valid Unicode".to_owned())?;
    if source_name == "." || source_name == ".." {
        return Err("source filename is not a valid ZIP entry".to_owned());
    }
    Ok(source_name)
}

fn zip_entry_names_collide(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    {
        let a = a.encode_utf16().collect::<Vec<_>>();
        let b = b.encode_utf16().collect::<Vec<_>>();
        return unsafe {
            windows::Win32::Globalization::CompareStringOrdinal(
                &a,
                &b,
                windows::Win32::Foundation::BOOL(1),
            )
        }
        .0 == 2;
    }
    #[cfg(not(windows))]
    {
        a.chars()
            .flat_map(char::to_lowercase)
            .eq(b.chars().flat_map(char::to_lowercase))
    }
}

fn validate_zip_create_facts(
    sources: &[PathBuf],
    destination: &Path,
    guard: &ZipArchiveCreateGuard,
) -> Result<(), String> {
    if sources.is_empty() || sources.len() > 1024 || sources.len() != guard.sources.len() {
        return Err("ZIP needs 1 to 1024 ordered source guards".into());
    }
    if !destination
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        return Err("ZIP destination must have a .zip extension".into());
    }
    let destination_parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "ZIP destination must have an explicit parent".to_owned())?;
    if guard.destination_parent_identity.windows_parts().is_none() {
        return Err("ZIP destination parent identity is not supported".into());
    }
    let mut names = Vec::<String>::with_capacity(sources.len());
    for (source, source_guard) in sources.iter().zip(&guard.sources) {
        let source_name = zip_entry_name(source)?;
        if source.parent() != Some(destination_parent) {
            return Err("ZIP sources and output must share one parent folder".into());
        }
        if source_guard.entry_name != source_name
            || !matches!(
                source_guard.source_kind,
                crate::directory::EntryKind::File | crate::directory::EntryKind::Folder
            )
            || source_guard.source_identity.windows_parts().is_none()
        {
            return Err("ZIP guard does not match its ordered source".into());
        }
        if names
            .iter()
            .any(|name| zip_entry_names_collide(name, source_name))
        {
            return Err("ZIP source entry names collide".into());
        }
        names.push(source_name.to_owned());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_zip_create_guard(source: &Path, destination: &Path) -> ZipCreateGuard {
    test_zip_create_guard_for_kind(source, destination, crate::directory::EntryKind::File)
}

pub(crate) fn test_zip_create_guard_for_kind(
    source: &Path,
    destination: &Path,
    source_kind: crate::directory::EntryKind,
) -> ZipCreateGuard {
    let source_identity =
        crate::directory::EntrySourceIdentity::from_path_no_follow(source, source_kind)
            .expect("ZIP source fixture identity")
            .filesystem_identity();
    let parent = destination.parent().expect("ZIP destination parent");
    let destination_parent_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
        parent,
        crate::directory::EntryKind::Folder,
    )
    .expect("ZIP destination parent fixture identity")
    .filesystem_identity();
    ZipCreateGuard {
        source_identity,
        destination_parent_identity,
        source_kind,
        entry_name: zip_entry_name(source)
            .expect("ZIP source entry name")
            .to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameReparseGuard {
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub reparse_tag: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveIdentityGuard {

    /// Empty only while a local OLE Move awaits worker-side source preflight.
    /// Once resolved, one identity for each source entry in the same order.
    pub source_identities: Vec<crate::filesystem_identity::FilesystemIdentity>,
    pub destination_identity: crate::filesystem_identity::FilesystemIdentity,
 }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCleanupIdentityGuard {
    /// CF_HDROP は path を渡すため、外部 drag 中に leaf が別 file へ差し替わりうる。
    /// cleanup 時は開いた handle の identity を照合し、差し替わった file は残す。
    pub source_identities: Vec<crate::filesystem_identity::FilesystemIdentity>,
    /// 各sourceの親経路を、pin中にhandleから取得した順序付きidentity。
    pub parent_routes: Vec<Vec<SourceCleanupParentIdentity>>,
    /// pin時leaf handleのsignature。hardlink差し戻し等の変化を検出し、判断不能なら削除しない。
    pub leaf_signatures: Vec<SourceCleanupLeafSignature>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceCleanupLeafSignature {
    /// Windows FILE_STANDARD_INFO.NumberOfLinks
    pub number_of_links: u32,
    /// Windows FILE_BASIC_INFO.ChangeTime の生値
    pub change_time: i64,
}

impl SourceCleanupLeafSignature {
    /// Cross-volume Move retry needs a source-change token the filesystem actually supplied.
    /// zero は変更時刻を検証材料として使えない filesystem 応答なので fail closed にする。
    pub fn is_reliable_for_cross_volume_move_retry(self) -> bool {
        self.number_of_links == 1 && self.change_time != 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCleanupParentIdentity {
    pub path: PathBuf,
    pub identity: crate::filesystem_identity::FilesystemIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationOrigin {
    pub view_id: u64,
    pub location_epoch: u64,
    pub presentation_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptState {
    Queued,
    Preparing,
    Running,
    WaitingConflict,
    CancelRequested,
    Terminal,
}

impl AttemptState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressCounters {
    pub processed_items: u64,
    /// Lower-bound count found while a folder tree is being traversed. This is
    /// not an exact total and may increase during the operation.
    pub discovered_items: u64,
    pub total_items: Option<u64>,
    pub processed_bytes: u64,
    /// Lower-bound byte sum of discovered regular files. Like
    /// `discovered_items`, it must never be presented as an exact total.
    pub discovered_bytes: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub sequence: u64,
    pub counters: ProgressCounters,
    pub current_item: Option<ItemId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemOutcome {
    Completed,
    Skipped,
    Failed,
    CancelledBeforeStart,
    Partial,
    Unknown,
}

/// A move receipt records the irreversible boundary separately from the
/// overall item outcome. In particular, a cross-volume move can publish its
/// destination but be unable (or no longer permitted) to remove the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveReceipt {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub destination_published: bool,
    pub source_removed: bool,
}

/// A replacement is a multi-step destructive operation. Its terminal fact
/// keeps the displaced object's owned backup explicit so recovery never has to
/// guess from a destination path alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaceReceipt {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub displaced_backup: Option<PathBuf>,
    pub destination_published: bool,
    pub displaced_backup_removed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemReceipt {
    pub item: ItemId,
    pub outcome: ItemOutcome,
    pub detail: Option<String>,
    pub created_folder_path: Option<PathBuf>,
    pub created_archive_path: Option<PathBuf>,
    pub move_receipt: Option<MoveReceipt>,
    pub replace_receipt: Option<ReplaceReceipt>,
    /// Exact ordinary-file Copy output facts; accepted only by the Runtime
    /// from a completed terminal receipt.
    pub copy_undo: Option<CopyUndoReceipt>,
}

pub const DIRECTORY_COPY_EVIDENCE_PREFIX: &str = "gpui-filer-directory-copy-evidence-v1:";
pub const DIRECTORY_COPY_STAGE_EVIDENCE_PREFIX: &str = "gpui-filer-directory-copy-stage-evidence-v1:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CopyDirectoryStagePhase { StagingFailed, StagingCancelled, CreatedRootIdentityUnverified }

/// Read-only diagnostic for an ordinary directory Copy whose unpublished stage was retained.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyDirectoryStageEvidence {
    pub stage_path_units: Vec<u16>,
    pub destination_path_units: Vec<u16>,
    pub expected_root_identity: String,
    pub phase: CopyDirectoryStagePhase,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CopyDirectoryPublicationPhase {
    RenameOutcomeUncertain,
    PostPublishCancelled,
    PublishedRootOpenFailed,
    PublishedRootIdentityFailed,
    PublishedInventoryFailed,
    PublishedInventoryMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopyDirectoryPublicationEvidence {
    /// Exact native UTF-16 path units; this record is inspection-only.
    pub stage_path_units: Vec<u16>,
    pub destination_path_units: Vec<u16>,
    pub expected_root_identity: String,
    pub expected_node_count: u64,
    pub expected_inventory_sha256: String,
    pub observed_root_identity: Option<String>,
    pub observed_node_count: Option<u64>,
    pub observed_inventory_sha256: Option<String>,
    pub phase: CopyDirectoryPublicationPhase,
    /// Bounded context for the phase failure; never a recovery instruction.
    pub detail: String,
}

/// Process-local evidence that one guarded same-volume Move returned an I/O
/// error from the direct no-replace adapter before any mutation occurred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedMoveNoEffectProof {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
}

/// Process-local proof that a direct ordinary cross-volume Move failed before
/// publication while its source and destination-parent handles were retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossVolumeMoveNoEffectProof {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_signature: SourceCleanupLeafSignature,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossVolumeMoveRetryGuard {
    pub proof: CrossVolumeMoveNoEffectProof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossVolumeMoveInspectionCandidate {
    pub proof: CrossVolumeMoveNoEffectProof,
    pub source_matches: bool,
    pub destination_parent_matches: bool,
    pub destination: GuardedMoveDestinationState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossVolumeMoveInspectionResult {
    pub ticket: InspectionTicket,
    pub attempt: AttemptToken,
    pub candidates: Vec<CrossVolumeMoveInspectionCandidate>,
}

/// Process-lifetime identity for one passive operation inspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InspectionTicket(u64);

impl InspectionTicket {
    pub fn value(self) -> u64 {
        self.0
    }
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedMoveInspectionCandidate {
    pub proof: GuardedMoveNoEffectProof,
    pub source_matches: bool,
    pub destination_parent_matches: bool,
    pub destination: GuardedMoveDestinationState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardedMoveDestinationState {
    Absent,
    Present,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedMoveInspectionResult {
    pub ticket: InspectionTicket,
    pub attempt: AttemptToken,
    pub candidates: Vec<GuardedMoveInspectionCandidate>,
}

/// Process-local evidence for the exact published-output/source-retained
/// outcome of an ordinary cross-volume file Move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedMoveSourceRetainedProof {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_signature: SourceCleanupLeafSignature,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_signature: SourceCleanupLeafSignature,
}

/// Worker evidence that a cross-volume Move accepted both output publication
/// and source disposition; it does not assert the current source path is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedMoveDispositionProof {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_signature: SourceCleanupLeafSignature,
    pub source_pre_disposition_signature: SourceCleanupLeafSignature,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_signature: SourceCleanupLeafSignature,
    pub source_parent_route: Vec<SourceCleanupParentIdentity>,
    pub destination_parent_route: Vec<SourceCleanupParentIdentity>,
    pub content_sha256: Option<String>,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishedMoveInspectionState { Matches, Changed, Absent, Indeterminate }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedMoveInspectionCandidate {
    pub proof: PublishedMoveSourceRetainedProof,
    pub source: PublishedMoveInspectionState,
    pub destination_parent: PublishedMoveInspectionState,
    pub destination: PublishedMoveInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedMoveInspectionResult {
    pub ticket: InspectionTicket,
    pub attempt: AttemptToken,
    pub synthesized_unknown: bool,
    pub candidates: Vec<PublishedMoveInspectionCandidate>,
}

/// Process-local facts emitted by the owned ordinary-file no-replace Copy adapter.
/// Missing observations stay `None`; they never imply that a path was absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyFailureProvenance {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub publication: CopyPublicationState,
    pub source_identity: Option<crate::filesystem_identity::FilesystemIdentity>,
    pub source_signature: Option<SourceCleanupLeafSignature>,
    pub source_signature_semantics: CopySourceSignatureSemantics,
    pub destination_parent_identity: Option<crate::filesystem_identity::FilesystemIdentity>,
    pub stage: Option<CopyStageObservation>,
}

/// Worker-time identity requirements captured by an exact Copy inspection.
/// The retry must reacquire these objects before creating any staging output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRetryGuard {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_signature: SourceCleanupLeafSignature,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
}

/// Worker-captured authority for retrying one ordinary directory Copy item.
/// It deliberately does not reuse the regular-file signature guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyDirectoryRetryGuard {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_root_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyDirectoryRetryProvenance {
    pub item: ItemId,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_root_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyPublicationState {
    NotPublished,
    Published,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopySourceSignatureSemantics {
    ChangeTimeAndLinkCountFromOpenHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyStageObservation {
    pub path: PathBuf,
    pub identity: Option<crate::filesystem_identity::FilesystemIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyInspectionState {
    Matches,
    Changed,
    Absent,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyFailureInspectionCandidate {
    pub provenance: CopyFailureProvenance,
    pub source: CopyInspectionState,
    pub destination_parent: CopyInspectionState,
    pub destination: CopyInspectionState,
    pub stage: CopyInspectionState,
    /// Stage identity is continuity evidence only; shared-write opens preclude byte stability.
    pub stage_content: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyFailureInspectionResult {
    pub ticket: InspectionTicket,
    pub attempt: AttemptToken,
    pub candidates: Vec<CopyFailureInspectionCandidate>,
    pub directory_candidates: Vec<CopyDirectoryInspectionCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyDirectoryInspectionCandidate {
    pub evidence: CopyDirectoryInspectionEvidence,
    pub stage_root: CopyInspectionState,
    pub destination_root: CopyInspectionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyDirectoryInspectionEvidence {
    Publication(CopyDirectoryPublicationEvidence),
    RetainedStage(CopyDirectoryStageEvidence),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyUndoReceipt {
    pub destination: PathBuf,
    pub parent_identity: String,
    pub file_identity: String,
    pub content_sha256: String,
    pub leaf_signature: SourceCleanupLeafSignature,
}

/// One exact member of a successful mixed ordinary Copy request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyUndoItem {
    File(CopyUndoReceipt),
    Directory(DirectoryCopyUndoReceipt),
}

/// Process-local proof captured from the same handle that created the object.
/// This receipt is deliberately absent from durable operation records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedObjectUndoReceipt {
    pub destination: PathBuf,
    pub parent_identity: String,
    pub object_identity: String,
    pub kind: CreatedObjectUndoKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveUndoReceipt {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub object_identity: crate::filesystem_identity::FilesystemIdentity,
    pub object_kind: MoveUndoObjectKind,
    pub source_parent_route: Vec<SourceCleanupParentIdentity>,
    pub destination_parent_route: Vec<SourceCleanupParentIdentity>,
    /// A present proof means the output is a copied object on another volume,
    /// so Undo must copy it back and then disposition this exact output handle.
    pub cross_volume_file: Option<CrossVolumeMoveUndoProof>,
}

/// Process-local proof for one completely published ordinary directory Copy.
/// It is deliberately separate from inspection evidence and durable records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryCopyUndoReceipt {
    pub destination: PathBuf,
    pub destination_parent_route: Vec<SourceCleanupParentIdentity>,
    pub root_identity: String,
    pub entries: Vec<DirectoryCopyUndoEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryCopyUndoEntry {
    pub relative_components: Vec<Vec<u16>>,
    pub kind: crate::operations::journal::JournalDirectoryNodeKind,
    pub identity: String,
    pub signature: Option<SourceCleanupLeafSignature>,
    pub length: u64,
    pub content_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossVolumeMoveUndoProof {
    pub original_source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub original_source_signature: SourceCleanupLeafSignature,
    pub destination_signature: SourceCleanupLeafSignature,
    pub content_sha256: String,
    /// Present only for a completed direct ordinary directory Move. The
    /// sibling fields remain file-only evidence and are intentionally empty.
    pub directory_manifest: Option<super::journal::JournalDirectoryMove>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveUndoObjectKind {
    RegularFile,
    Directory,
    CaseOnlyRename,
    /// Exact supported link object moved by a same-volume no-follow rename.
    ReparsePoint { tag: u32, is_directory: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreatedObjectUndoKind {
    EmptyFile {
        content_sha256: String,
        signature: SourceCleanupLeafSignature,
    },
    ShortcutFile {
        content_sha256: String,
        signature: SourceCleanupLeafSignature,
        size: u64,
    },
    EmptyFolder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Partial,
    Unknown,
}

/// Retry is explicit so E1 cannot silently replay completed or unknown work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryPlan {
    NotAllowed,
    Explicit(Vec<ItemId>),
    RequiresReinspection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsChangeHint {
    pub directory: PathBuf,
}

/// A terminal report is self-contained; it does not rely on prior progress delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalReport {
    pub outcome: TerminalOutcome,
    pub final_counters: ProgressCounters,
    pub receipts: Vec<ItemReceipt>,
    pub retry: RetryPlan,
    pub change_hints: Vec<FsChangeHint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictNotice {
    pub item: ItemId,
    pub probe_revision: u64,
    pub description: String,
    pub replace_supported: bool,
    pub(crate) directory_move_replace_rename_candidate: Option<DirectoryMoveReplaceRenameCandidate>,
}

/// Worker-captured evidence for a future same-volume ordinary-directory Replace.
/// This stays inside the operation owner and is never projected to the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectoryMoveReplaceRenameCandidate {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_identity: crate::filesystem_identity::FilesystemIdentity,
    pub approved_target_identity: crate::filesystem_identity::FilesystemIdentity,
    pub volume_identity: u32,
    pub source_parent: PathBuf,
    pub source_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub destination_parent: PathBuf,
    pub destination_parent_identity: crate::filesystem_identity::FilesystemIdentity,
    pub source_is_ordinary_directory: bool,
    pub target_is_ordinary_directory: bool,
    pub approved_target_children: Vec<DirectoryMoveReplaceRenameCandidateChild>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectoryMoveReplaceRenameCandidateChild {
    pub ordinal: u32,
    pub node_kind: DirectoryMoveReplaceRenameCandidateNodeKind,
    pub relative_components: Vec<Vec<u16>>,
    pub identity: String,
    pub content_fact: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectoryMoveReplaceRenameCandidateNodeKind {
    File,
    Directory,
}

const DIRECTORY_MOVE_REPLACE_MAX_ENTRIES: usize = 1_024;
const DIRECTORY_MOVE_REPLACE_MAX_DEPTH: usize = 128;
const DIRECTORY_MOVE_REPLACE_MAX_RELATIVE_NAME_BYTES: usize = 2 * 1024 * 1024;
const DIRECTORY_MOVE_REPLACE_MAX_SERIALIZED_BYTES: usize = 3 * 1024 * 1024;

fn candidate_manifest_valid(candidate: &DirectoryMoveReplaceRenameCandidate) -> bool {
    if candidate.approved_target_children.len() > DIRECTORY_MOVE_REPLACE_MAX_ENTRIES {
        return false;
    }
    let mut previous_depth = usize::MAX;
    let mut previous_path: Option<Vec<String>> = None;
    let mut relative_name_bytes = 0usize;
    let mut serialized_bound = 512usize;
    for path in [
        &candidate.source,
        &candidate.destination,
        &candidate.source_parent,
        &candidate.destination_parent,
    ] {
        serialized_bound = serialized_bound.saturating_add(path.as_os_str().len().saturating_mul(6));
    }
    for identity in [
        &candidate.source_identity,
        &candidate.approved_target_identity,
        &candidate.source_parent_identity,
        &candidate.destination_parent_identity,
    ] {
        serialized_bound = serialized_bound.saturating_add(format!("{identity:?}").len());
    }

    for (index, child) in candidate.approved_target_children.iter().enumerate() {
        if child.ordinal as usize != index
            || child.identity.is_empty()
            || child.content_fact.is_empty()
            || child.relative_components.is_empty()
            || child.relative_components.len() > DIRECTORY_MOVE_REPLACE_MAX_DEPTH
            || child.relative_components.iter().any(|component| {
                component.is_empty()
                    || component == &[b'.' as u16]
                    || component == &[b'.' as u16, b'.' as u16]
                    || component.iter().any(|unit| matches!(*unit, 0 | 0x2f | 0x5c))
                    || String::from_utf16(component).is_err()
            })
        {
            return false;
        }

        let child_name_bytes = child.relative_components.iter().fold(0usize, |total, part| {
            total.saturating_add(part.len().saturating_mul(2))
        });
        relative_name_bytes = relative_name_bytes.saturating_add(child_name_bytes);
        serialized_bound = serialized_bound
            .saturating_add(1_024)
            .saturating_add(child_name_bytes.saturating_mul(3))
            .saturating_add(child.identity.len().saturating_mul(6))
            .saturating_add(child.content_fact.len().saturating_mul(6));

        let depth = child.relative_components.len();
        let Some(path) = child.relative_components.iter()
            .map(|part| String::from_utf16(part).ok().map(|component| component.to_lowercase()))
            .collect::<Option<Vec<_>>>() else {
            return false;
        };
        if depth > previous_depth
            || (depth == previous_depth && previous_path.as_ref().is_some_and(|previous| previous >= &path))
        {
            return false;
        }
        previous_depth = depth;
        previous_path = Some(path);
    }

    relative_name_bytes <= DIRECTORY_MOVE_REPLACE_MAX_RELATIVE_NAME_BYTES
        && serialized_bound <= DIRECTORY_MOVE_REPLACE_MAX_SERIALIZED_BYTES
}

fn candidate_path_eq(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right
}

fn candidate_windows_ordinal_component_eq(
    left: &std::ffi::OsStr,
    right: &std::ffi::OsStr,
) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let left = left.encode_wide().collect::<Vec<_>>();
        let right = right.encode_wide().collect::<Vec<_>>();
        // Treat only equality or API failure as a possible path overlap.
        matches!(
            unsafe {
                windows::Win32::Globalization::CompareStringOrdinal(
                    &left,
                    &right,
                    windows::Win32::Foundation::BOOL(1),
                )
            }
            .0,
            0 | 2
        )
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn candidate_path_prefix(parent: &std::path::Path, path: &std::path::Path) -> bool {
    let parent = parent.components().map(|part| part.as_os_str().to_owned()).collect::<Vec<_>>();
    let path = path.components().map(|part| part.as_os_str().to_owned()).collect::<Vec<_>>();
    parent.len() <= path.len()
        && parent.iter().zip(path.iter()).all(|(a, b)| {
            candidate_windows_ordinal_component_eq(a, b)
        })
}

fn candidate_valid_for_request(
    candidate: &DirectoryMoveReplaceRenameCandidate,
    request: &OperationRequest,
    item: ItemId,
) -> bool {
    if !candidate_manifest_valid(candidate) {
        return false;
    }
    let (Some(source), Some(requested_parent)) = (request.sources.first(), request.destination.as_ref()) else { return false; };
    let Some(source_leaf) = source.file_name() else { return false; };
    let expected_destination = requested_parent.join(source_leaf);
    let Some(source_parent) = source.parent() else { return false; };
    let Some((source_volume, _)) = candidate.source_identity.windows_parts() else { return false; };
    let Some((target_volume, _)) = candidate.approved_target_identity.windows_parts() else { return false; };
    let Some((source_parent_volume, _)) = candidate.source_parent_identity.windows_parts() else { return false; };
    let Some((destination_parent_volume, _)) = candidate.destination_parent_identity.windows_parts() else { return false; };
    request.kind == OperationKind::Move
        && request.virtual_source.is_none()
        && request.sources.len() == 1
        && item == ItemId(0)
        && candidate.source_is_ordinary_directory
        && candidate.target_is_ordinary_directory
        && candidate.volume_identity == source_volume
        && source_volume == target_volume
        && source_volume == source_parent_volume
        && source_volume == destination_parent_volume
        && candidate_path_eq(source, &candidate.source)
        && candidate_path_eq(source_parent, &candidate.source_parent)
        && candidate_path_eq(requested_parent, &candidate.destination_parent)
        && candidate_path_eq(&expected_destination, &candidate.destination)
        && candidate.source.is_absolute()
        && candidate.destination.is_absolute()
        && candidate.source_parent.is_absolute()
        && candidate.destination_parent.is_absolute()
        && candidate.source_identity != candidate.approved_target_identity
        && !(candidate_path_prefix(&candidate.source, &candidate.destination)
            || candidate_path_prefix(&candidate.destination, &candidate.source))
        && !candidate_path_prefix(&candidate.source, &candidate.destination_parent)
        && request.move_identity_guard.as_ref().is_none_or(|guard| {
            guard.source_identities.len() == 1
                && guard.source_identities[0] == candidate.source_identity
                && guard.destination_identity == candidate.destination_parent_identity
        })
        && request.move_destination_identity_guard.as_ref().is_none_or(|identity| {
            identity == &candidate.destination_parent_identity
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictDecision {
    Skip,
    KeepBoth,
    Replace,
}

/// Commands to a future executor. Progress is intentionally not represented here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutorCommand {
    Prepare {
        token: AttemptToken,
        request: OperationRequest,
        retry_items: Vec<ItemId>,
        copy_retry_guards: Vec<CopyRetryGuard>,
        copy_directory_retry_guards: Vec<CopyDirectoryRetryGuard>,
        cross_volume_move_retry_guards: Vec<CrossVolumeMoveRetryGuard>,
    },
    Cancel {
        token: AttemptToken,
    },
    ResolveConflict {
        token: ConflictToken,
        decision: ConflictDecision,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceEvent {
    TerminalCommitted {
        token: AttemptToken,
        report: TerminalReport,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    CapacityReached,
    IdExhausted,
    IdentityUnavailable,
    InvalidRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryError {
    MissingJob,
    NotTerminal,
    NotRetryable,
    CapacityReached,
    IdExhausted,
}

#[derive(Clone, Debug)]
struct PendingConflict {
    token: ConflictToken,
    description: String,
    replace_supported: bool,
    directory_move_replace_rename_candidate: Option<DirectoryMoveReplaceRenameCandidate>,
}

#[derive(Clone, Debug)]
struct Attempt {
    token: AttemptToken,
    state: AttemptState,
    held: bool,
    retry_items: Vec<ItemId>,
    last_progress_sequence: Option<u64>,
    progress: Option<ProgressUpdate>,
    pending_conflict: Option<PendingConflict>,
    accepted_directory_move_replace_rename: Option<(ConflictToken, DirectoryMoveReplaceRenameCandidate)>,
    terminal: Option<TerminalReport>,
    guarded_move_no_effect_proofs: Vec<GuardedMoveNoEffectProof>,
    cross_volume_move_no_effect_proofs: Vec<CrossVolumeMoveNoEffectProof>,
    published_move_source_retained: Vec<PublishedMoveSourceRetainedProof>,
    published_move_disposition: Vec<PublishedMoveDispositionProof>,
    copy_failure_provenance: Vec<CopyFailureProvenance>,
    invalid_copy_failure_retry_provenance: HashSet<ItemId>,
    copy_directory_retry_provenance: Vec<CopyDirectoryRetryProvenance>,
    invalid_copy_directory_retry_provenance: HashSet<ItemId>,
    copy_retry_guards: Vec<CopyRetryGuard>,
    copy_directory_retry_guards: Vec<CopyDirectoryRetryGuard>,
    cross_volume_move_retry_guards: Vec<CrossVolumeMoveRetryGuard>,
    next_conflict: u64,
}

#[derive(Clone, Debug)]
struct Job {
    request: OperationRequest,
    attempts: Vec<Attempt>,
}

/// Immutable inspection projection for UI/tests. It cannot mutate the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptSnapshot {
    pub token: AttemptToken,
    pub state: AttemptState,
    pub progress: Option<ProgressUpdate>,
    pub pending_conflict: Option<PendingConflictSnapshot>,
    pub terminal: Option<TerminalReport>,
    pub retry_items: Vec<ItemId>,
    pub copy_directory_retry_provenance: Vec<CopyDirectoryRetryProvenance>,
    pub copy_retry_guards: Vec<CopyRetryGuard>,
    pub copy_directory_retry_guards: Vec<CopyDirectoryRetryGuard>,
    pub invalid_copy_failure_retry_items: Vec<ItemId>,
    pub invalid_copy_directory_retry_items: Vec<ItemId>,
}

/// Immutable conflict detail for the presentation layer. The token remains
/// opaque and must be echoed unchanged when a decision is submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingConflictSnapshot {
    pub token: ConflictToken,
    pub description: String,
    pub replace_supported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSnapshot {
    pub id: JobId,
    pub request: OperationRequest,
    pub attempts: Vec<AttemptSnapshot>,
}

/// Compact disposable row data for the live progress presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressRowSnapshot {
    pub token: AttemptToken,
    pub kind: OperationKind,
    pub state: AttemptState,
    pub terminal_outcome: Option<TerminalOutcome>,
    /// A visible Retry action is safe only when the current terminal attempt carries explicit work.
    pub retryable: bool,
    pub counters: Option<ProgressCounters>,
}

/// The only mutable owner of accepted operation jobs.
///
/// No filesystem, GPUI entity, directory snapshot, or watcher lives here.
pub struct OperationService {
    capacity: usize,
    next_job: u64,
    next_attempt: u64,
    jobs: BTreeMap<JobId, Job>,
}

impl Default for OperationService {
    fn default() -> Self {
        Self::new(64)
    }
}

impl OperationService {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            next_job: 1,
            next_attempt: 1,
            jobs: BTreeMap::new(),
        }
    }

    pub fn submit(&mut self, request: OperationRequest) -> Result<AttemptToken, SubmitError> {
        self.submit_with_hold(request, false)
    }

    /// Admits an attempt against normal capacity while withholding it from the executor queue.
    pub fn submit_held(&mut self, request: OperationRequest) -> Result<AttemptToken, SubmitError> {
        self.submit_with_hold(request, true)
    }

    fn submit_with_hold(
        &mut self,
        request: OperationRequest,
        held: bool,
    ) -> Result<AttemptToken, SubmitError> {
        if request.clipboard_move_source_evidence.as_ref().is_some_and(|evidence| {
            request.kind != OperationKind::Move
                || request.virtual_source.is_some()
                || evidence.is_empty()
                || evidence.len() != request.sources.len()
                || request.move_destination_identity_guard.is_none()
                || request.move_identity_guard.is_some()
                || evidence.iter().zip(&request.sources).any(|(accepted, source)| {
                    accepted.source != *source
                        || !source.is_absolute()
                        || accepted.source_identity.windows_parts().is_none()
                        || accepted.reparse_tag.is_some_and(|tag| !matches!(tag, 0xA000_000C | 0xA000_0003))
                        || (accepted.reparse_tag == Some(0xA000_0003)
                            && accepted.source_kind != crate::directory::EntryKind::Folder)
                })
        }) {
            return Err(SubmitError::InvalidRequest);
        }
        if let (Some(sources), Some(replace_sources)) = (
            request.clipboard_move_source_evidence.as_ref(),
            request.move_replace_source_evidence.as_ref(),
        ) && (sources.len() != replace_sources.len()
            || sources.iter().zip(replace_sources).any(|(source, replace)| {
                source.source_identity != replace.source_identity
            }))
        {
            return Err(SubmitError::InvalidRequest);
        }
        if self.active_job_count() >= self.capacity {
            return Err(SubmitError::CapacityReached);
        }
        let job = JobId(self.take_job_id()?);
        let token = AttemptToken {
            job,
            attempt: AttemptId(self.take_attempt_id()?),
        };
        self.jobs.insert(
            job,
            Job {
                request,
                attempts: vec![Attempt::queued(token, Vec::new(), held)],
            },
        );
        Ok(token)
    }

    /// Releases only this exact held attempt. Repeated or stale releases are harmless.
    pub fn release_held(&mut self, token: AttemptToken) -> bool {
        let Some(attempt) = self.current_attempt_mut(token) else {
            return false;
        };
        if attempt.state != AttemptState::Queued || !attempt.held {
            return false;
        }
        attempt.held = false;
        true
    }

    /// durable admission のACK前に実行へ流れないよう、まだqueuedのheld jobだけを取り除く。
    pub fn reject_held(&mut self, token: AttemptToken) -> bool {
        let eligible = self.jobs.get(&token.job_id()).is_some_and(|job| {
            job.attempts.last().is_some_and(|attempt| {
                attempt.token == token
                    && attempt.state == AttemptState::Queued
                    && attempt.held
            })
        });
        if !eligible {
            return false;
        }
        self.jobs.remove(&token.job_id()).is_some()
    }

    /// Persists the worker's Submit-path preflight result on the owning job so
    /// every retry uses the same source identities.
    pub fn resolve_move_identity_guard(
        &mut self,
        token: AttemptToken,
        guard: MoveIdentityGuard,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else {
            return false;
        };
        let Some(attempt) = job.attempts.last() else {
            return false;
        };
        if attempt.token != token
            || !matches!(
                attempt.state,
                AttemptState::Preparing | AttemptState::Running
            )
            || job.request.kind != OperationKind::Move
            || guard.source_identities.len() != job.request.sources.len()
            || guard.source_identities.is_empty()
            || job
                .request
                .move_identity_guard
                .as_ref()
                .is_none_or(|pending| {
                    !pending.source_identities.is_empty()
                        || pending.destination_identity != guard.destination_identity
                })
        {
            return false;
        }
        job.request.move_identity_guard = Some(guard);
        true
    }

    /// Retains only one proof per item, after checking it against the exact
    /// current Move request and its accepted Submit-path identity guard.
    pub fn record_guarded_move_no_effect_proof(
        &mut self,
        token: AttemptToken,
        proof: GuardedMoveNoEffectProof,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else {
            return false;
        };
        if job.request.kind != OperationKind::Move {
            return false;
        }
        let Some(source) = usize::try_from(proof.item.0)
            .ok()
            .and_then(|index| job.request.sources.get(index))
        else {
            return false;
        };
        let Some(destination) = job
            .request
            .destination
            .as_ref()
            .zip(source.file_name())
            .map(|(parent, name)| parent.join(name))
        else {
            return false;
        };
        let Some(guard) = job.request.move_identity_guard.as_ref() else {
            return false;
        };
        let source_identity_matches = guard
            .source_identities
            .get(proof.item.0 as usize)
            .is_some_and(|identity| *identity == proof.source_identity);
        let Some(attempt) = job.attempts.last_mut() else {
            return false;
        };
        if attempt.token != token
            || !matches!(
                attempt.state,
                AttemptState::Preparing | AttemptState::Running | AttemptState::CancelRequested
            )
            || proof.source != *source
            || proof.destination != destination
            || !source_identity_matches
            || proof.destination_parent_identity != guard.destination_identity
            || attempt
                .guarded_move_no_effect_proofs
                .iter()
                .any(|old| old.item == proof.item)
        {
            return false;
        }
        attempt.guarded_move_no_effect_proofs.push(proof);
        true
    }

    /// Returns passive evidence only for the exact terminal attempt token.
    pub fn guarded_move_no_effect_proofs(
        &self,
        token: AttemptToken,
    ) -> Option<Vec<GuardedMoveNoEffectProof>> {
        let job = self.jobs.get(&token.job)?;
        let attempt = job.attempts.iter().find(|attempt| attempt.token == token)?;
        (attempt.state == AttemptState::Terminal)
            .then(|| attempt.guarded_move_no_effect_proofs.clone())
    }

    pub fn record_cross_volume_move_no_effect_proof(
        &mut self,
        token: AttemptToken,
        proof: CrossVolumeMoveNoEffectProof,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else { return false; };
        if job.request.kind != OperationKind::Move || job.request.virtual_source.is_some()
            || job.request.move_destination_identity_guard.is_some() { return false; }
        let Some(source) = usize::try_from(proof.item.0).ok().and_then(|i| job.request.sources.get(i)) else { return false; };
        let Some(destination) = job.request.destination.as_ref().zip(source.file_name()).map(|(root, name)| root.join(name)) else { return false; };
        let Some(attempt) = job.attempts.last_mut() else { return false; };
        if attempt.token != token
            || !matches!(attempt.state, AttemptState::Preparing | AttemptState::Running | AttemptState::CancelRequested)
            || proof.source != *source || proof.destination != destination
            || proof.source_identity.windows_parts().is_none()
            || proof.destination_parent_identity.windows_parts().is_none()
            || !proof.source_signature.is_reliable_for_cross_volume_move_retry()
            || attempt.cross_volume_move_no_effect_proofs.iter().any(|p| p.item == proof.item) { return false; }
        attempt.cross_volume_move_no_effect_proofs.push(proof);
        true
    }

    pub fn cross_volume_move_no_effect_proofs(&self, token: AttemptToken) -> Option<Vec<CrossVolumeMoveNoEffectProof>> {
        let job = self.jobs.get(&token.job)?;
        let attempt = job.attempts.iter().find(|attempt| attempt.token == token)?;
        (attempt.state == AttemptState::Terminal).then(|| attempt.cross_volume_move_no_effect_proofs.clone())
    }

    pub fn record_published_move_source_retained(
        &mut self,
        token: AttemptToken,
        proof: PublishedMoveSourceRetainedProof,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else { return false; };
        if job.request.kind != OperationKind::Move { return false; }
        let Some(source) = usize::try_from(proof.item.0).ok().and_then(|i| job.request.sources.get(i)) else { return false; };
        let Some(destination) = job.request.destination.as_ref().and_then(|root| source.file_name().map(|name| root.join(name))) else { return false; };
        let destination_matches = proof.destination == destination || (2..10_000_u32).any(|suffix| {
            keep_both_candidate_path(&destination, suffix).as_ref() == Some(&proof.destination)
        });
        let Some(attempt) = job.attempts.last_mut() else { return false; };
        if attempt.token != token
            || !matches!(attempt.state, AttemptState::Preparing | AttemptState::Running | AttemptState::CancelRequested)
            || proof.source != *source || !destination_matches
            || attempt.published_move_source_retained.iter().any(|old| old.item == proof.item)
        { return false; }
        attempt.published_move_source_retained.push(proof);
        true
    }

    pub fn published_move_source_retained(&self, token: AttemptToken) -> Option<Vec<PublishedMoveSourceRetainedProof>> {
        let job = self.jobs.get(&token.job)?;
        let attempt = job.attempts.iter().find(|attempt| attempt.token == token)?;
        (attempt.state == AttemptState::Terminal && attempt.terminal.as_ref().is_some_and(|r| r.outcome == TerminalOutcome::Partial))
            .then(|| attempt.published_move_source_retained.clone())
    }

    pub fn record_published_move_disposition(&mut self, token: AttemptToken, proof: PublishedMoveDispositionProof) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else { return false; };
        if job.request.kind != OperationKind::Move { return false; }
        let Some(source) = usize::try_from(proof.item.0).ok().and_then(|i| job.request.sources.get(i)) else { return false; };
        let Some(destination) = job.request.destination.as_ref().and_then(|root| source.file_name().map(|name| root.join(name))) else { return false; };
        let matches_destination = proof.destination == destination || (2..10_000_u32).any(|suffix| keep_both_candidate_path(&destination, suffix).as_ref() == Some(&proof.destination));
        let Some(attempt) = job.attempts.last_mut() else { return false; };
        if attempt.token != token || !matches!(attempt.state, AttemptState::Preparing | AttemptState::Running | AttemptState::CancelRequested)
            || proof.source != *source || !matches_destination
            || proof.source_parent_route.last().is_none_or(|parent| proof.source.parent() != Some(parent.path.as_path()))
            || proof.destination_parent_route.last().is_none_or(|parent| proof.destination.parent() != Some(parent.path.as_path()))
            || proof.source_parent_route.is_empty() || proof.destination_parent_route.is_empty()
            || attempt.published_move_disposition.iter().any(|old| old.item == proof.item) { return false; }
        attempt.published_move_disposition.push(proof);
        true
    }

    pub fn published_move_disposition(&self, token: AttemptToken) -> Option<Vec<PublishedMoveDispositionProof>> {
        let job = self.jobs.get(&token.job)?;
        let attempt = job.attempts.iter().find(|attempt| attempt.token == token)?;
        (attempt.token == token).then(|| attempt.published_move_disposition.clone())
    }


    /// Claims one queued attempt for the single initial executor lane.
    pub fn start_next(&mut self) -> Option<ExecutorCommand> {
        for (job_id, job) in &mut self.jobs {
            let Some(attempt) = job.attempts.last_mut() else {
                continue;
            };
            if attempt.state == AttemptState::Queued && !attempt.held {
                attempt.state = AttemptState::Preparing;
                return Some(ExecutorCommand::Prepare {
                    token: AttemptToken {
                        job: *job_id,
                        attempt: attempt.token.attempt,
                    },
                    request: job.request.clone(),
                    retry_items: attempt.retry_items.clone(),
                    copy_retry_guards: attempt.copy_retry_guards.clone(),
                    copy_directory_retry_guards: attempt.copy_directory_retry_guards.clone(),
                    cross_volume_move_retry_guards: attempt.cross_volume_move_retry_guards.clone(),
                });
            }
        }
        None
    }

    /// The executor has completed preflight and may now report work progress.
    pub fn prepared(&mut self, token: AttemptToken) -> bool {
        let Some(attempt) = self.current_attempt_mut(token) else {
            return false;
        };
        if attempt.state != AttemptState::Preparing {
            return false;
        }
        attempt.state = AttemptState::Running;
        true
    }

    /// Latest-wins progress mailbox commit. Duplicate/out-of-order updates are rejected.
    pub fn progress(&mut self, token: AttemptToken, update: ProgressUpdate) -> bool {
        let Some(attempt) = self.current_attempt_mut(token) else {
            return false;
        };
        if !matches!(
            attempt.state,
            AttemptState::Preparing | AttemptState::Running
        ) || attempt
            .last_progress_sequence
            .is_some_and(|previous| update.sequence <= previous)
        {
            return false;
        }
        attempt.last_progress_sequence = Some(update.sequence);
        attempt.progress = Some(update);
        true
    }

    /// Raises one conflict and freezes normal execution until its exact token is answered.
    pub fn conflict(
        &mut self,
        token: AttemptToken,
        mut notice: ConflictNotice,
    ) -> Option<ConflictToken> {
        if let Some(candidate) = &notice.directory_move_replace_rename_candidate {
            let valid = self.jobs.get(&token.job)
                .and_then(|job| job.attempts.last().filter(|attempt| attempt.token == token).map(|_| &job.request))
                .is_some_and(|request| candidate_valid_for_request(candidate, request, notice.item))
                && notice.replace_supported;
            if !valid {
                notice.directory_move_replace_rename_candidate = None;
                notice.replace_supported = false;
            }
        }
        let attempt = self.current_attempt_mut(token)?;
        if attempt.state != AttemptState::Running || attempt.pending_conflict.is_some() {
            return None;
        }
        let conflict = attempt.next_conflict.checked_add(1)?;
        attempt.next_conflict = conflict;
        let token = ConflictToken {
            attempt: token,
            item: notice.item,
            conflict,
            probe_revision: notice.probe_revision,
        };
        attempt.pending_conflict = Some(PendingConflict {
            token,
            description: notice.description,
            replace_supported: notice.replace_supported,
            directory_move_replace_rename_candidate: notice.directory_move_replace_rename_candidate,
        });
        attempt.state = AttemptState::WaitingConflict;
        Some(token)
    }

    /// Rejects stale answers rather than applying them to another probe or retry.
    pub fn can_resolve_conflict(&self, token: ConflictToken, decision: ConflictDecision) -> bool {
        let Some(attempt) = self
            .jobs
            .get(&token.attempt.job)
            .and_then(|job| job.attempts.last())
            .filter(|attempt| attempt.token == token.attempt)
        else {
            return false;
        };
        attempt.state == AttemptState::WaitingConflict
            && attempt.pending_conflict.as_ref().is_some_and(|pending| {
                // Replace の可否は exact target を probe した worker の観測値に従う。
                pending.token == token
                    && (decision != ConflictDecision::Replace || pending.replace_supported)
            })
    }

    pub fn resolve_conflict(
        &mut self,
        token: ConflictToken,
        decision: ConflictDecision,
    ) -> Option<ExecutorCommand> {
        if !self.can_resolve_conflict(token, decision) {
            return None;
        }
        let attempt = self.current_attempt_mut(token.attempt)?;
        let candidate = attempt.pending_conflict.as_ref()
            .filter(|pending| pending.token == token)
            .and_then(|pending| pending.directory_move_replace_rename_candidate.clone());
        attempt.pending_conflict = None;
        attempt.accepted_directory_move_replace_rename = if decision == ConflictDecision::Replace {
            candidate.map(|candidate| (token, candidate))
        } else {
            None
        };
        attempt.state = AttemptState::Running;
        Some(ExecutorCommand::ResolveConflict { token, decision })
    }

    pub(crate) fn accepted_directory_move_replace_rename(
        &self,
        token: ConflictToken,
    ) -> Option<DirectoryMoveReplaceRenameCandidate> {
        let attempt = self.jobs.get(&token.attempt.job)?.attempts.last()?;
        (attempt.token == token.attempt && matches!(attempt.state, AttemptState::Running | AttemptState::CancelRequested))
            .then(|| attempt.accepted_directory_move_replace_rename.as_ref())
            .flatten()
            .filter(|(accepted_token, _)| *accepted_token == token)
            .map(|(_, candidate)| candidate.clone())
    }

    /// Dialog の明示 cancel/escape。表示していた完全 token がまだ pending の
    /// conflict と一致するときだけ、worker へ cancel を送る。
    pub fn cancel_conflict(&mut self, token: ConflictToken) -> Option<ExecutorCommand> {
        let attempt = self.current_attempt_mut(token.attempt)?;
        if attempt.state != AttemptState::WaitingConflict
            || attempt
                .pending_conflict
                .as_ref()
                .map(|pending| pending.token)
                != Some(token)
        {
            return None;
        }
        attempt.pending_conflict = None;
        // Keep the exact approved E7 candidate until its already-authorized
        // pre-displacement journal transition settles. Terminal clears it.
        attempt.state = AttemptState::CancelRequested;
        Some(ExecutorCommand::Cancel {
            token: token.attempt,
        })
    }

    /// Requests cancellation once. Only a later terminal report freezes the result.
    pub fn cancel(&mut self, token: AttemptToken) -> Option<ServiceEventOrCommand> {
        let is_create_zip = self.job(token.job_id()).is_some_and(|job| {
            matches!(
                job.request.kind,
                OperationKind::CreateZip | OperationKind::Create7z
            ) && job.request.virtual_source.is_none()
        });
        let virtual_source_totals = self
            .job(token.job_id())
            .and_then(|job| job.request.virtual_source)
            .map(|sources| {
                let discovered_bytes = sources
                    .items
                    .iter()
                    .filter_map(|source| source.size)
                    .fold(0_u64, u64::saturating_add);
                let total_bytes = sources
                    .items
                    .iter()
                    .try_fold(0_u64, |total, source| total.checked_add(source.size?));
                (sources.items.len(), total_bytes, discovered_bytes)
            });
        let attempt = self.current_attempt_mut(token)?;
        match attempt.state {
            AttemptState::Queued => {
                attempt.held = false;
                attempt.state = AttemptState::Terminal;
                let report = cancelled_before_start_report(virtual_source_totals, is_create_zip);
                attempt.terminal = Some(report.clone());
                Some(ServiceEventOrCommand::Event(
                    ServiceEvent::TerminalCommitted { token, report },
                ))
            }
            AttemptState::Preparing | AttemptState::Running | AttemptState::WaitingConflict => {
                attempt.pending_conflict = None;
                // E7 may still need this candidate to validate a durable
                // NoEffect fact after cancellation races a phase write.
                attempt.state = AttemptState::CancelRequested;
                Some(ServiceEventOrCommand::Command(ExecutorCommand::Cancel {
                    token,
                }))
            }
            AttemptState::CancelRequested | AttemptState::Terminal => None,
        }
    }

    /// Commits exactly one reliable terminal report. Late or retry-stale completions are ignored.
    pub fn terminal(
        &mut self,
        token: AttemptToken,
        report: TerminalReport,
    ) -> Option<ServiceEvent> {
        let attempt = self.current_attempt_mut(token)?;
        if !matches!(
            attempt.state,
            AttemptState::Preparing
                | AttemptState::Running
                | AttemptState::WaitingConflict
                | AttemptState::CancelRequested
        ) {
            return None;
        }
        attempt.pending_conflict = None;
        attempt.accepted_directory_move_replace_rename = None;
        attempt.state = AttemptState::Terminal;
        attempt.terminal = Some(report.clone());
        attempt.held = false;
        Some(ServiceEvent::TerminalCommitted { token, report })
    }

    /// Creates a fresh queued attempt from an explicit terminal retry plan.
    pub fn retry(&mut self, expected: AttemptToken) -> Result<AttemptToken, RetryError> {
        let job_id = expected.job_id();
        let retry_items = {
            let job = self.jobs.get(&job_id).ok_or(RetryError::MissingJob)?;
            let attempt = job.attempts.last().ok_or(RetryError::MissingJob)?;
            if attempt.token != expected {
                return Err(RetryError::NotRetryable);
            }
            if attempt.state != AttemptState::Terminal {
                return Err(RetryError::NotTerminal);
            }
            match attempt.terminal.as_ref().map(|report| &report.retry) {
                Some(RetryPlan::Explicit(items)) if !items.is_empty() => items.clone(),
                _ => return Err(RetryError::NotRetryable),
            }
        };
        if self.active_job_count() >= self.capacity {
            return Err(RetryError::CapacityReached);
        }
        let token = AttemptToken {
            job: job_id,
            attempt: AttemptId(
                self.take_attempt_id()
                    .map_err(|_| RetryError::IdExhausted)?,
            ),
        };
        self.jobs
            .get_mut(&job_id)
            .expect("job was checked above")
            .attempts
            .push(Attempt::queued(token, retry_items, false));
        Ok(token)
    }

    pub fn record_copy_failure_provenance(
        &mut self,
        token: AttemptToken,
        provenance: CopyFailureProvenance,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else {
            return false;
        };
        if job.request.kind != OperationKind::Copy {
            return false;
        }
        let Some(source) = usize::try_from(provenance.item.0)
            .ok()
            .and_then(|i| job.request.sources.get(i))
        else {
            return false;
        };
        let Some(attempt) = job.attempts.last_mut() else {
            return false;
        };
        if attempt.token != token
            || !matches!(
                attempt.state,
                AttemptState::Preparing | AttemptState::Running | AttemptState::CancelRequested
            )
        {
            return false;
        }
        if attempt.copy_failure_provenance.iter().any(|old| old.item == provenance.item) {
            attempt.invalid_copy_failure_retry_provenance.insert(provenance.item);
            return false;
        }
        let Some(destination) = job
            .request
            .destination
            .as_ref()
            .zip(source.file_name())
            .map(|(parent, name)| parent.join(name))
        else {
            attempt.invalid_copy_failure_retry_provenance.insert(provenance.item);
            return false;
        };
        if provenance.source != *source
            || (provenance.destination != destination
                && !is_keep_both_candidate_path(&destination, &provenance.destination))
        {
            attempt.invalid_copy_failure_retry_provenance.insert(provenance.item);
            return false;
        }
        attempt.copy_failure_provenance.push(provenance);
        true
    }

    pub fn record_copy_directory_retry_provenance(
        &mut self,
        token: AttemptToken,
        provenance: CopyDirectoryRetryProvenance,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&token.job) else { return false; };
        let Some(source) = usize::try_from(provenance.item.0).ok().and_then(|i| job.request.sources.get(i)) else { return false; };
        let Some(destination) = job.request.destination.as_ref().zip(source.file_name()).map(|(parent, name)| parent.join(name)) else { return false; };
        let Some(attempt) = job.attempts.last_mut() else { return false; };
        if job.request.kind != OperationKind::Copy || job.request.virtual_source.is_some()
            || attempt.token != token || !matches!(attempt.state, AttemptState::Preparing | AttemptState::Running | AttemptState::CancelRequested)
            || provenance.source != *source || provenance.destination != destination
            || provenance.source_root_identity.windows_parts().is_none()
            || provenance.destination_parent_identity.windows_parts().is_none()
            || attempt.copy_failure_provenance.iter().any(|old| old.item == provenance.item)
            || attempt.invalid_copy_directory_retry_provenance.contains(&provenance.item) { return false; }
        if attempt.copy_directory_retry_provenance.iter().any(|old| old.item == provenance.item) {
            attempt.invalid_copy_directory_retry_provenance.insert(provenance.item);
            return false;
        }
        attempt.copy_directory_retry_provenance.push(provenance);
        true
    }

    pub fn copy_failure_provenance(
        &self,
        token: AttemptToken,
    ) -> Option<Vec<CopyFailureProvenance>> {
        let job = self.jobs.get(&token.job)?;
        let attempt = job.attempts.iter().find(|attempt| attempt.token == token)?;
        (attempt.state == AttemptState::Terminal).then(|| attempt.copy_failure_provenance.clone())
    }

    #[cfg(test)]
    pub(crate) fn replace_terminal_report_for_test(
        &mut self,
        token: AttemptToken,
        report: TerminalReport,
    ) -> bool {
        let Some(attempt) = self
            .jobs
            .get_mut(&token.job)
            .and_then(|job| job.attempts.last_mut())
        else {
            return false;
        };
        if attempt.token != token || attempt.state != AttemptState::Terminal {
            return false;
        }
        attempt.terminal = Some(report);
        true
    }

    /// Creates a retry from owner-validated evidence without changing the terminal report.
    pub(crate) fn retry_verified_items(
        &mut self,
        expected: AttemptToken,
        retry_items: Vec<ItemId>,
    ) -> Result<AttemptToken, RetryError> {
        if retry_items.is_empty() {
            return Err(RetryError::NotRetryable);
        }
        let job_id = expected.job_id();
        let attempt = self
            .jobs
            .get(&job_id)
            .and_then(|job| job.attempts.last())
            .ok_or(RetryError::MissingJob)?;
        if attempt.token != expected {
            return Err(RetryError::NotRetryable);
        }
        if attempt.state != AttemptState::Terminal {
            return Err(RetryError::NotTerminal);
        }
        if self.active_job_count() >= self.capacity {
            return Err(RetryError::CapacityReached);
        }
        let token = AttemptToken {
            job: job_id,
            attempt: AttemptId(
                self.take_attempt_id()
                    .map_err(|_| RetryError::IdExhausted)?,
            ),
        };
        self.jobs
            .get_mut(&job_id)
            .expect("job checked above")
            .attempts
            .push(Attempt::queued(token, retry_items, false));
        Ok(token)
    }

    pub(crate) fn retry_verified_copy_items(
        &mut self,
        expected: AttemptToken,
        guards: Vec<CopyRetryGuard>,
    ) -> Result<AttemptToken, RetryError> {
        if guards.is_empty() {
            return Err(RetryError::NotRetryable);
        }
        let job_id = expected.job_id();
        let job = self.jobs.get(&job_id).ok_or(RetryError::MissingJob)?;
        if job.request.kind != OperationKind::Copy
            || job.request.virtual_source.is_some()
            || job.request.copy_destination_identity_guard.is_some()
        {
            return Err(RetryError::NotRetryable);
        }
        let destination_root = job.request.destination.as_deref();
        let mut items = Vec::with_capacity(guards.len());
        let mut seen = HashSet::new();
        for guard in &guards {
            let Some(source) = job.request.sources.get(guard.item.0 as usize) else {
                return Err(RetryError::NotRetryable);
            };
            if !seen.insert(guard.item)
                || source != &guard.source
                || destination_root != guard.destination.parent()
                || source.file_name() != guard.destination.file_name()
                || guard.source_identity.windows_parts().is_none()
                || guard.destination_parent_identity.windows_parts().is_none()
            {
                return Err(RetryError::NotRetryable);
            }
            items.push(guard.item);
        }
        let token = self.retry_verified_items(expected, items)?;
        let attempt = self
            .jobs
            .get_mut(&job_id)
            .and_then(|job| job.attempts.last_mut())
            .expect("verified retry appends one attempt");
        attempt.copy_retry_guards = guards;
        Ok(token)
    }

    pub(crate) fn retry_verified_copy_items_with_directory_guards(
        &mut self,
        expected: AttemptToken,
        file_guards: Vec<CopyRetryGuard>,
        directory_guards: Vec<CopyDirectoryRetryGuard>,
        guarded_file_classification: Option<Vec<ItemId>>,
        guarded_directory_classification: Option<Vec<ItemId>>,
    ) -> Result<AttemptToken, RetryError> {
        if file_guards.is_empty() && directory_guards.is_empty() { return Err(RetryError::NotRetryable); }
        let job_id = expected.job_id();
        let job = self.jobs.get(&job_id).ok_or(RetryError::MissingJob)?;
        if job.request.kind != OperationKind::Copy || job.request.virtual_source.is_some()
            || job.request.move_identity_guard.is_some()
            || job.request.move_destination_identity_guard.is_some()
            || job.request.move_replace_source_evidence.is_some()
            || job.request.rename_reparse_guard.is_some()
            || job.request.source_cleanup_identity_guard.is_some()
            || job.request.initial_conflict_policy.is_some()
            || job.request.batch_rename_plan.is_some()
        { return Err(RetryError::NotRetryable); }
        let destination_identity_guard = job.request.copy_destination_identity_guard.as_ref();
        if destination_identity_guard.is_some_and(|guard| guard.windows_parts().is_none()) {
            return Err(RetryError::NotRetryable);
        }
        let invalid_guarded_route = if destination_identity_guard.is_some() {
            let (Some(file_classification), Some(directory_classification)) =
                (guarded_file_classification.as_ref(), guarded_directory_classification.as_ref())
            else { return Err(RetryError::NotRetryable); };
            let mut classified = HashSet::new();
            let classes_are_complete = file_classification.iter().all(|item| classified.insert(*item))
                && directory_classification.iter().all(|item| classified.insert(*item))
                && classified.len() == job.request.sources.len()
                && (0..job.request.sources.len()).all(|index| u32::try_from(index).ok()
                    .is_some_and(|index| classified.contains(&ItemId(index))))
                && !job.request.sources.is_empty();
            !classes_are_complete
                || file_guards.iter().any(|guard| !file_classification.contains(&guard.item))
                || directory_guards.iter().any(|guard| !directory_classification.contains(&guard.item))
                || (file_guards.is_empty() && directory_guards.is_empty())
        } else {
            guarded_file_classification.is_some() || guarded_directory_classification.is_some()
        };
        if invalid_guarded_route { return Err(RetryError::NotRetryable); }
        let mut items = Vec::with_capacity(file_guards.len() + directory_guards.len());
        let mut seen = HashSet::new();
        for guard in &file_guards {
            let Some(source) = job.request.sources.get(guard.item.0 as usize) else { return Err(RetryError::NotRetryable); };
            let Some(destination) = job.request.destination.as_deref().zip(source.file_name())
                .map(|(parent, name)| parent.join(name)) else { return Err(RetryError::NotRetryable); };
            if !seen.insert(guard.item) || source != &guard.source || destination != guard.destination
                || job.request.destination.as_deref() != guard.destination.parent()
                || source.file_name() != guard.destination.file_name()
                || guard.source_identity.windows_parts().is_none()
                || guard.destination_parent_identity.windows_parts().is_none()
                || destination_identity_guard.is_some_and(|identity| identity != &guard.destination_parent_identity)
            { return Err(RetryError::NotRetryable); }
            items.push(guard.item);
        }
        for guard in &directory_guards {
            let Some(source) = job.request.sources.get(guard.item.0 as usize) else { return Err(RetryError::NotRetryable); };
            let Some(destination) = job.request.destination.as_deref().zip(source.file_name())
                .map(|(parent, name)| parent.join(name)) else { return Err(RetryError::NotRetryable); };
            if !seen.insert(guard.item) || source != &guard.source || job.request.destination.as_deref() != guard.destination.parent()
                || source.file_name() != guard.destination.file_name() || guard.destination != destination
                || guard.source_root_identity.windows_parts().is_none()
                || guard.destination_parent_identity.windows_parts().is_none()
                || destination_identity_guard.is_some_and(|identity| identity != &guard.destination_parent_identity)
            { return Err(RetryError::NotRetryable); }
            items.push(guard.item);
        }
        if destination_identity_guard.is_some() {
            let retry_items = items.iter().copied().collect::<HashSet<_>>();
            if retry_items.len() != items.len()
                || retry_items.len() != file_guards.len() + directory_guards.len()
                || !retry_items.iter().all(|item| {
                    guarded_file_classification.as_ref().is_some_and(|items| items.contains(item))
                        && file_guards.iter().any(|guard| guard.item == *item)
                    || guarded_directory_classification.as_ref().is_some_and(|items| items.contains(item))
                        && directory_guards.iter().any(|guard| guard.item == *item)
                })
            { return Err(RetryError::NotRetryable); }
        }
        let token = self.retry_verified_items(expected, items)?;
        let attempt = self.jobs.get_mut(&job_id).and_then(|job| job.attempts.last_mut()).expect("verified Copy retry appends one attempt");
        attempt.copy_retry_guards = file_guards;
        attempt.copy_directory_retry_guards = directory_guards;
        Ok(token)
    }

    pub(crate) fn retry_verified_cross_volume_move_items(
        &mut self,
        expected: AttemptToken,
        guards: Vec<CrossVolumeMoveRetryGuard>,
    ) -> Result<AttemptToken, RetryError> {
        if guards.is_empty() { return Err(RetryError::NotRetryable); }
        let job_id = expected.job_id();
        let job = self.jobs.get(&job_id).ok_or(RetryError::MissingJob)?;
        if job.request.kind != OperationKind::Move || job.request.virtual_source.is_some()
            || job.request.move_destination_identity_guard.is_some() { return Err(RetryError::NotRetryable); }
        let mut items = Vec::with_capacity(guards.len());
        let mut seen = HashSet::new();
        for guard in &guards {
            let proof = &guard.proof;
            let Some(source) = job.request.sources.get(proof.item.0 as usize) else { return Err(RetryError::NotRetryable); };
            let Some(destination) = job.request.destination.as_deref().zip(source.file_name()).map(|(parent, name)| parent.join(name)) else { return Err(RetryError::NotRetryable); };
            if !seen.insert(proof.item) || source != &proof.source || destination != proof.destination
                || proof.source_identity.windows_parts().is_none() || proof.destination_parent_identity.windows_parts().is_none()
                || !proof.source_signature.is_reliable_for_cross_volume_move_retry() { return Err(RetryError::NotRetryable); }
            items.push(proof.item);
        }
        let token = self.retry_verified_items(expected, items)?;
        self.jobs.get_mut(&job_id).and_then(|job| job.attempts.last_mut())
            .expect("verified retry appends one attempt").cross_volume_move_retry_guards = guards;
        Ok(token)
    }

    pub fn job(&self, id: JobId) -> Option<JobSnapshot> {
        let job = self.jobs.get(&id)?;
        Some(JobSnapshot {
            id,
            request: job.request.clone(),
            attempts: job
                .attempts
                .iter()
                .map(|attempt| AttemptSnapshot {
                    token: attempt.token,
                    state: attempt.state,
                    progress: attempt.progress.clone(),
                    pending_conflict: attempt.pending_conflict.as_ref().map(|pending| {
                        PendingConflictSnapshot {
                            token: pending.token,
                            description: pending.description.clone(),
                            replace_supported: pending.replace_supported,
                        }
                    }),
                    terminal: attempt.terminal.clone(),
                    retry_items: attempt.retry_items.clone(),
                    copy_directory_retry_provenance: attempt.copy_directory_retry_provenance.clone(),
                    copy_retry_guards: attempt.copy_retry_guards.clone(),
                    copy_directory_retry_guards: attempt.copy_directory_retry_guards.clone(),
                    invalid_copy_failure_retry_items: attempt.invalid_copy_failure_retry_provenance.iter().copied().collect(),
                    invalid_copy_directory_retry_items: attempt.invalid_copy_directory_retry_provenance.iter().copied().collect(),
                })
                .collect(),
        })
    }

    pub(crate) fn cross_volume_retry_authority(
        &self,
        token: AttemptToken,
    ) -> Option<(Vec<ItemId>, Vec<CrossVolumeMoveRetryGuard>)> {
        let attempt = self.jobs.get(&token.job_id())?.attempts.iter().find(|attempt| attempt.token == token)?;
        (!attempt.cross_volume_move_retry_guards.is_empty()).then(|| {
            (attempt.retry_items.clone(), attempt.cross_volume_move_retry_guards.clone())
        })
    }

    /// Read-only presentation projection. `BTreeMap` order makes the result
    /// deterministic without allowing a view to retain or mutate job state.
    pub fn jobs(&self) -> Vec<JobSnapshot> {
        self.jobs.keys().filter_map(|id| self.job(*id)).collect()
    }

    /// Read-only newest-first presentation projection bounded before snapshots are cloned.
    pub fn recent_jobs(&self, limit: usize) -> Vec<JobSnapshot> {
        self.jobs
            .keys()
            .rev()
            .take(limit)
            .filter_map(|id| self.job(*id))
            .collect()
    }

    pub fn progress_rows(&self) -> Vec<ProgressRowSnapshot> {
        self.jobs.values().filter_map(|job| {
            let attempt = job.attempts.last()?;
            let terminal_outcome = attempt.terminal.as_ref().map(|report| report.outcome);
            if matches!(terminal_outcome, Some(TerminalOutcome::Succeeded | TerminalOutcome::Cancelled)) {
                return None;
            }
            Some(ProgressRowSnapshot {
                token: attempt.token,
                kind: job.request.kind,
                state: attempt.state,
                terminal_outcome,
                retryable: attempt.state == AttemptState::Terminal
                    && attempt.terminal.as_ref().is_some_and(|report| {
                        matches!(&report.retry, RetryPlan::Explicit(items) if !items.is_empty())
                    }),
                counters: attempt.progress.as_ref().map(|progress| progress.counters),
            })
        }).collect()
    }

    pub fn has_progress_rows(&self) -> bool {
        self.jobs.values().any(|job| {
            job.attempts.last().is_some_and(|attempt| {
                !matches!(
                    attempt.terminal.as_ref().map(|report| report.outcome),
                    Some(TerminalOutcome::Succeeded | TerminalOutcome::Cancelled)
                )
            })
        })
    }

    fn current_attempt_mut(&mut self, token: AttemptToken) -> Option<&mut Attempt> {
        let job = self.jobs.get_mut(&token.job)?;
        let attempt = job.attempts.last_mut()?;
        (attempt.token == token && !attempt.state.is_terminal()).then_some(attempt)
    }

    fn active_job_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| {
                job.attempts
                    .last()
                    .is_some_and(|attempt| !attempt.state.is_terminal())
            })
            .count()
    }

    fn take_job_id(&mut self) -> Result<u64, SubmitError> {
        let id = self.next_job;
        self.next_job = self
            .next_job
            .checked_add(1)
            .ok_or(SubmitError::IdExhausted)?;
        Ok(id)
    }

    fn take_attempt_id(&mut self) -> Result<u64, SubmitError> {
        let id = self.next_attempt;
        self.next_attempt = self
            .next_attempt
            .checked_add(1)
            .ok_or(SubmitError::IdExhausted)?;
        Ok(id)
    }
}

impl Attempt {
    fn queued(token: AttemptToken, retry_items: Vec<ItemId>, held: bool) -> Self {
        Self {
            token,
            state: AttemptState::Queued,
            held,
            retry_items,
            last_progress_sequence: None,
            progress: None,
            pending_conflict: None,
            accepted_directory_move_replace_rename: None,
            terminal: None,
            guarded_move_no_effect_proofs: Vec::new(),
            cross_volume_move_no_effect_proofs: Vec::new(),
            published_move_source_retained: Vec::new(),
            published_move_disposition: Vec::new(),
            copy_failure_provenance: Vec::new(),
            invalid_copy_failure_retry_provenance: HashSet::new(),
            copy_directory_retry_provenance: Vec::new(),
            invalid_copy_directory_retry_provenance: HashSet::new(),
            copy_retry_guards: Vec::new(),
            copy_directory_retry_guards: Vec::new(),
            cross_volume_move_retry_guards: Vec::new(),
            next_conflict: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceEventOrCommand {
    Event(ServiceEvent),
    Command(ExecutorCommand),
}

fn cancelled_before_start_report(
    virtual_source_totals: Option<(usize, Option<u64>, u64)>,
    create_zip: bool,
) -> TerminalReport {
    let mut final_counters = ProgressCounters::default();
    let receipts = if create_zip {
        final_counters.total_items = Some(1);
        vec![ItemReceipt {
            item: ItemId(0),
            outcome: ItemOutcome::CancelledBeforeStart,
            detail: None,
            created_folder_path: None,
            created_archive_path: None,
            move_receipt: None,
            replace_receipt: None,
            copy_undo: None,
        }]
    } else {
        virtual_source_totals.map_or_else(Vec::new, |(count, total_bytes, discovered_bytes)| {
            final_counters.discovered_items = count as u64;
            final_counters.total_items = Some(count as u64);
            final_counters.total_bytes = total_bytes;
            final_counters.discovered_bytes = discovered_bytes;
            (0..count)
                .map(|index| ItemReceipt {
                    item: ItemId(index as u32),
                    outcome: ItemOutcome::CancelledBeforeStart,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                })
                .collect()
        })
    };
    TerminalReport {
        outcome: TerminalOutcome::Cancelled,
        final_counters,
        receipts,
        retry: RetryPlan::NotAllowed,
        change_hints: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e7_candidate(source: &Path, destination: &Path) -> DirectoryMoveReplaceRenameCandidate {
        let source_parent = source.parent().unwrap().to_path_buf();
        let destination_parent = destination.parent().unwrap().to_path_buf();
        DirectoryMoveReplaceRenameCandidate {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 1),
            approved_target_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            volume_identity: 1,
            source_parent,
            source_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 3),
            destination_parent,
            destination_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 4),
            source_is_ordinary_directory: true,
            target_is_ordinary_directory: true,
            approved_target_children: Vec::new(),
        }
    }

    fn e7_move_request(source: &Path, destination_parent: &Path) -> OperationRequest {
        let mut request = request();
        request.kind = OperationKind::Move;
        request.sources = vec![source.to_path_buf()];
        request.destination = Some(destination_parent.to_path_buf());
        request.move_identity_guard = Some(MoveIdentityGuard {
            source_identities: vec![crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 1)],
            destination_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 4),
        });
        request
    }

    #[test]
    fn e7_candidate_allows_nested_source_but_rejects_descendant_parent_and_root_overlap() {
        let nested_source = Path::new(r"C:\destination\nested\incoming");
        let destination_parent = Path::new(r"C:\destination");
        let nested_target = destination_parent.join("incoming");
        let candidate = e7_candidate(nested_source, &nested_target);
        let request = e7_move_request(nested_source, destination_parent);
        assert!(candidate_valid_for_request(&candidate, &request, ItemId(0)));

        let source = Path::new(r"C:\root");
        let child_destination_parent = Path::new(r"C:\root\child");
        let descendant_target = child_destination_parent.join("root");
        let candidate = e7_candidate(source, &descendant_target);
        let request = e7_move_request(source, child_destination_parent);
        assert!(!candidate_valid_for_request(&candidate, &request, ItemId(0)));

        let source = Path::new(r"C:\destination\incoming\nested\incoming");
        let destination_parent = Path::new(r"C:\destination");
        let overlapping_target = destination_parent.join("incoming");
        let candidate = e7_candidate(source, &overlapping_target);
        let request = e7_move_request(source, destination_parent);
        assert!(!candidate_valid_for_request(&candidate, &request, ItemId(0)));
    }

    #[test]
    fn zip_entry_name_rejects_non_unicode_without_lossy_conversion() {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let invalid = std::ffi::OsString::from_wide(&[0xD800]);
            assert!(zip_entry_name(Path::new(&invalid)).is_err());
        }
    }

    #[test]
    fn create_zip_captures_only_an_ordinary_file_and_zip_target() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("資料.txt");
        std::fs::write(&source, b"data").unwrap();
        let output = directory.path().join("資料.zip");
        let guard = test_zip_create_guard(&source, &output);
        let request = OperationRequest::create_zip(source.clone(), output.clone(), guard).unwrap();
        assert_eq!(request.kind, OperationKind::CreateZip);
        assert_eq!(
            match request.zip_create_guard.as_ref().unwrap() {
                ZipOperationGuard::Create(guard) => guard.sources[0].entry_name.as_str(),
                _ => unreachable!(),
            },
            "資料.txt"
        );
        let bad_output = directory.path().join("bad.tar");
        assert!(
            OperationRequest::create_zip(
                source.clone(),
                bad_output.clone(),
                test_zip_create_guard(&source, &bad_output)
            )
            .is_err()
        );
        let mismatched = ZipCreateGuard {
            entry_name: "other.txt".to_owned(),
            ..test_zip_create_guard(&source, &output)
        };
        assert!(OperationRequest::create_zip(source, output, mismatched).is_err());
    }

    #[test]
    fn create_zip_accepts_captured_folder_identity_and_full_dotted_leaf() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("project.backup");
        std::fs::create_dir(&source).unwrap();
        let output = parent.path().join("project.backup.zip");
        let mut guard =
            test_zip_create_guard_for_kind(&source, &output, crate::directory::EntryKind::Folder);
        guard.entry_name = "project.backup".to_owned();
        let request = OperationRequest::create_zip(source.clone(), output.clone(), guard).unwrap();
        assert_eq!(
            match request.zip_create_guard.as_ref().unwrap() {
                ZipOperationGuard::Create(guard) => guard.sources[0].source_kind,
                _ => unreachable!(),
            },
            crate::directory::EntryKind::Folder
        );
        assert_eq!(
            match request.zip_create_guard.as_ref().unwrap() {
                ZipOperationGuard::Create(guard) => guard.sources[0].entry_name.as_str(),
                _ => unreachable!(),
            },
            "project.backup"
        );
    }

    #[test]
    fn create_zip_many_requires_exact_ordered_sibling_facts_and_ordinal_unique_names() {
        let parent = tempfile::tempdir().unwrap();
        let first = parent.path().join("Å.txt");
        let second = parent.path().join("other.txt");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();
        let output = parent.path().join("Å.zip");
        let first_guard = test_zip_create_guard(&first, &output);
        let second_guard = test_zip_create_guard(&second, &output);
        let guard = ZipArchiveCreateGuard {
            destination_parent_identity: first_guard.destination_parent_identity.clone(),
            sources: vec![
                ZipCreateSourceGuard {
                    source_identity: first_guard.source_identity.clone(),
                    source_kind: first_guard.source_kind,
                    entry_name: "Å.txt".into(),
                },
                ZipCreateSourceGuard {
                    source_identity: second_guard.source_identity.clone(),
                    source_kind: second_guard.source_kind,
                    entry_name: "other.txt".into(),
                },
            ],
        };
        let request = OperationRequest::create_zip_many(
            vec![first.clone(), second.clone()],
            output.clone(),
            guard.clone(),
        )
        .unwrap();
        assert_eq!(request.sources, vec![first.clone(), second.clone()]);
        assert!(request.zip_create_shape_is_valid());

        let mut truncated = guard.clone();
        truncated.sources.pop();
        assert!(
            OperationRequest::create_zip_many(
                vec![first.clone(), second.clone()],
                output.clone(),
                truncated
            )
            .is_err()
        );
        let mut reordered = guard.clone();
        reordered.sources.swap(0, 1);
        assert!(
            OperationRequest::create_zip_many(
                vec![first.clone(), second.clone()],
                output.clone(),
                reordered
            )
            .is_err()
        );
        let mut mismatched_leaf = guard.clone();
        mismatched_leaf.sources[1].entry_name = "different.txt".into();
        assert!(
            OperationRequest::create_zip_many(
                vec![first.clone(), second.clone()],
                output.clone(),
                mismatched_leaf
            )
            .is_err()
        );
        assert!(
            OperationRequest::create_zip_many(
                vec![first.clone(), parent.path().join("elsewhere/third")],
                output.clone(),
                guard.clone()
            )
            .is_err()
        );

        let mut bad_source_identity = guard.clone();
        bad_source_identity.sources[1].source_identity =
            crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                "unsupported".into(),
            );
        assert!(
            OperationRequest::create_zip_many(
                vec![first.clone(), second.clone()],
                output.clone(),
                bad_source_identity
            )
            .is_err()
        );
        let mut bad_parent_identity = guard.clone();
        bad_parent_identity.destination_parent_identity =
            crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                "unsupported-parent".into(),
            );
        assert!(
            OperationRequest::create_zip_many(
                vec![first.clone(), second.clone()],
                output.clone(),
                bad_parent_identity
            )
            .is_err()
        );

        let lower = parent.path().join("å.txt");
        std::fs::write(&lower, b"case collision").unwrap();
        let lower_guard = test_zip_create_guard(&lower, &output);
        let colliding = ZipArchiveCreateGuard {
            destination_parent_identity: first_guard.destination_parent_identity,
            sources: vec![
                ZipCreateSourceGuard {
                    source_identity: first_guard.source_identity,
                    source_kind: crate::directory::EntryKind::File,
                    entry_name: "Å.txt".into(),
                },
                ZipCreateSourceGuard {
                    source_identity: lower_guard.source_identity,
                    source_kind: crate::directory::EntryKind::File,
                    entry_name: "å.txt".into(),
                },
            ],
        };
        assert!(OperationRequest::create_zip_many(vec![first, lower], output, colliding).is_err());
    }

    fn seven_zip_request(parent: &Path) -> OperationRequest {
        let first = parent.join("Å.txt");
        let second = parent.join("other.txt");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();
        let output = parent.join("result.7z");
        let first_guard = test_zip_create_guard(&first, &output);
        let second_guard = test_zip_create_guard(&second, &output);
        let provider_hint = |name: &str, file_index| crate::archive_provider::ArchiveFileHint {
            path: parent.join(name),
            identity: crate::archive_provider::ArchiveFileIdentity {
                volume_serial: 1,
                file_index,
            },
        };
        OperationRequest::create_7z(
            vec![first, second],
            output,
            SevenZipCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity.clone(),
                sources: vec![first_guard, second_guard]
                    .into_iter()
                    .map(|guard| SevenZipCreateSourceGuard {
                        source_identity: guard.source_identity,
                        source_kind: guard.source_kind,
                        entry_name: guard.entry_name,
                    })
                    .collect(),
                provider_revision: 42,
                provider_binding: crate::archive_provider::ArchiveOperationBinding {
                    executable: provider_hint("7z.exe", 2),
                    zip_dll: provider_hint("7z.dll", 3),
                },
            },
        )
        .unwrap()
    }

    #[test]
    fn create_7z_constructor_captures_ordered_roots_and_rejects_bad_shape() {
        let parent = tempfile::tempdir().unwrap();
        let request = seven_zip_request(parent.path());
        assert_eq!(request.kind, OperationKind::Create7z);
        assert_eq!(
            request
                .seven_zip_create_guard
                .as_ref()
                .unwrap()
                .sources
                .len(),
            2
        );
        assert!(request.seven_zip_create_shape_is_valid());

        let mut bad_extension = request.clone();
        bad_extension.destination = Some(parent.path().join("result.zip"));
        assert!(!bad_extension.seven_zip_create_shape_is_valid());
        let mut unrelated_guard = request.clone();
        unrelated_guard.kind = OperationKind::Copy;
        assert!(!unrelated_guard.seven_zip_create_shape_is_valid());
        let mut zip_guard = request;
        zip_guard.zip_create_guard = Some(ZipOperationGuard::Extract(test_zip_create_guard(
            zip_guard.sources[0].as_path(),
            zip_guard.destination.as_ref().unwrap(),
        )));
        assert!(!zip_guard.seven_zip_create_shape_is_valid());
    }

    #[test]
    fn extract_7z_constructor_preserves_order_and_rejects_bad_direct_facts() {
        let parent = tempfile::tempdir().unwrap();
        let first = parent.path().join("Å.7z");
        let second = parent.path().join("other.7z");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();
        let first_identity = test_zip_create_guard(&first, &parent.path().join("Å"));
        let second_identity = test_zip_create_guard(&second, &parent.path().join("other"));
        let provider_hint = |name: &str, file_index| crate::archive_provider::ArchiveFileHint {
            path: parent.path().join(name),
            identity: crate::archive_provider::ArchiveFileIdentity {
                volume_serial: 1,
                file_index,
            },
        };
        let make_guard = || SevenZipExtractGuard {
            destination_parent_identity: first_identity.destination_parent_identity.clone(),
            sources: vec![
                SevenZipExtractSourceGuard {
                    source_identity: first_identity.source_identity.clone(),
                    source_kind: crate::directory::EntryKind::File,
                    entry_name: "Å.7z".into(),
                },
                SevenZipExtractSourceGuard {
                    source_identity: second_identity.source_identity.clone(),
                    source_kind: crate::directory::EntryKind::File,
                    entry_name: "other.7z".into(),
                },
            ],
            provider_revision: 77,
            provider_binding: crate::archive_provider::ArchiveOperationBinding {
                executable: provider_hint("7z.exe", 22),
                zip_dll: provider_hint("7z.dll", 23),
            },
        };
        let request =
            OperationRequest::extract_7z_many(vec![first.clone(), second.clone()], make_guard())
                .unwrap();
        assert_eq!(request.kind, OperationKind::Extract7z);
        assert_eq!(
            request.destination.as_deref(),
            Some(parent.path().join("Å").as_path())
        );
        assert!(request.seven_zip_extract_shape_is_valid());

        let singleton = OperationRequest::extract_7z_many(
            vec![first.clone()],
            SevenZipExtractGuard {
                sources: vec![make_guard().sources[0].clone()],
                ..make_guard()
            },
        )
        .unwrap();
        assert_eq!(singleton.sources.len(), 1);

        let mut wrong_order = make_guard();
        wrong_order.sources.swap(0, 1);
        assert!(
            OperationRequest::extract_7z_many(vec![first.clone(), second.clone()], wrong_order)
                .is_err()
        );
        let mut wrong_kind = make_guard();
        wrong_kind.sources[0].source_kind = crate::directory::EntryKind::Folder;
        assert!(
            OperationRequest::extract_7z_many(vec![first.clone(), second.clone()], wrong_kind)
                .is_err()
        );
        let mut duplicate_identity = make_guard();
        duplicate_identity.sources[1].source_identity =
            duplicate_identity.sources[0].source_identity.clone();
        assert!(
            OperationRequest::extract_7z_many(
                vec![first.clone(), second.clone()],
                duplicate_identity
            )
            .is_err()
        );
        let mut wrong_provider = make_guard();
        wrong_provider.provider_revision = 0;
        assert!(
            OperationRequest::extract_7z_many(vec![first.clone(), second.clone()], wrong_provider)
                .is_err()
        );
        let mut mixed = request.clone();
        mixed.zip_create_guard = Some(ZipOperationGuard::Extract(test_zip_create_guard(
            &first,
            &parent.path().join("Å"),
        )));
        assert!(!mixed.seven_zip_extract_shape_is_valid());

        let lower = parent.path().join("å.7z");
        std::fs::write(&lower, b"lower").unwrap();
        let lower_identity = test_zip_create_guard(&lower, &parent.path().join("å"));
        let mut collision_guard = make_guard();
        collision_guard.sources[1] = SevenZipExtractSourceGuard {
            source_identity: lower_identity.source_identity,
            source_kind: crate::directory::EntryKind::File,
            entry_name: "å.7z".into(),
        };
        assert!(
            OperationRequest::extract_7z_many(vec![first.clone(), lower], collision_guard).is_err()
        );

        std::fs::remove_file(&first).unwrap();
        std::fs::remove_file(&second).unwrap();
        assert!(
            OperationRequest::extract_7z_many(vec![first, second], make_guard()).is_ok(),
            "request construction consumes captured facts and performs no filesystem query"
        );
    }

    #[test]
    fn create_7z_constructor_rejects_nonordinary_names_and_paths_but_keeps_unicode() {
        let parent = tempfile::tempdir().unwrap();
        let valid = seven_zip_request(parent.path());
        let mut guard = valid.seven_zip_create_guard.clone().unwrap();
        guard.sources[0].entry_name = "COM¹.txt".into();
        let mut invalid_leaf_sources = valid.sources.clone();
        invalid_leaf_sources[0] = parent.path().join("COM¹.txt");
        assert!(
            OperationRequest::create_7z(
                invalid_leaf_sources,
                valid.destination.clone().unwrap(),
                guard
            )
            .is_err()
        );

        let mut guard = valid.seven_zip_create_guard.clone().unwrap();
        guard.provider_binding.executable.path = PathBuf::from("relative/7z.exe");
        assert!(
            OperationRequest::create_7z(
                valid.sources.clone(),
                valid.destination.clone().unwrap(),
                guard
            )
            .is_err()
        );

        let mut guard = valid.seven_zip_create_guard.clone().unwrap();
        guard.sources[0].entry_name = "bad\u{1f}name".into();
        let mut invalid_control_sources = valid.sources.clone();
        invalid_control_sources[0] = parent.path().join("bad\u{1f}name");
        assert!(
            OperationRequest::create_7z(
                invalid_control_sources,
                valid.destination.clone().unwrap(),
                guard
            )
            .is_err()
        );

        let guard = valid.seven_zip_create_guard.clone().unwrap();
        assert!(
            OperationRequest::create_7z(
                vec![PathBuf::from("relative.txt"), valid.sources[1].clone()],
                parent.path().join("relative-output.7z"),
                guard,
            )
            .is_err()
        );

        let first = parent.path().join("café-日本.txt");
        let second = parent.path().join("résumé-資料.txt");
        let output = parent.path().join("résultat.7z");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let first_guard = test_zip_create_guard(&first, &output);
        let second_guard = test_zip_create_guard(&second, &output);
        let provider_hint = |name: &str, file_index| crate::archive_provider::ArchiveFileHint {
            path: parent.path().join(name),
            identity: crate::archive_provider::ArchiveFileIdentity {
                volume_serial: 1,
                file_index,
            },
        };
        let guard = SevenZipCreateGuard {
            destination_parent_identity: first_guard.destination_parent_identity.clone(),
            sources: vec![first_guard, second_guard]
                .into_iter()
                .map(|facts| SevenZipCreateSourceGuard {
                    source_identity: facts.source_identity,
                    source_kind: facts.source_kind,
                    entry_name: facts.entry_name,
                })
                .collect(),
            provider_revision: 42,
            provider_binding: crate::archive_provider::ArchiveOperationBinding {
                executable: provider_hint("7z.exe", 2),
                zip_dll: provider_hint("7z.dll", 3),
            },
        };
        assert!(OperationRequest::create_7z(vec![first, second], output, guard).is_ok());
    }

    #[test]
    fn queued_7z_cancel_has_one_output_receipt() {
        let parent = tempfile::tempdir().unwrap();
        let mut service = OperationService::new(2);
        let token = service.submit(seven_zip_request(parent.path())).unwrap();
        let Some(ServiceEventOrCommand::Event(ServiceEvent::TerminalCommitted {
            token: actual,
            report,
        })) = service.cancel(token)
        else {
            panic!("queued 7z cancellation must commit without starting a worker");
        };
        assert_eq!(actual, token);
        assert_eq!(report.final_counters.total_items, Some(1));
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(report.receipts[0].item, ItemId(0));
        assert_eq!(
            report.receipts[0].outcome,
            ItemOutcome::CancelledBeforeStart
        );
    }

    fn request() -> OperationRequest {
        OperationRequest {
            kind: OperationKind::Copy,
            virtual_source: None,
            sources: vec![PathBuf::from(r"C:\source.txt")],
            destination: Some(PathBuf::from(r"C:\target")),
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
            origin: Some(OperationOrigin {
                view_id: 4,
                location_epoch: 7,
                presentation_revision: 9,
            }),
            batch_rename_plan: None,
        }
    }

    fn report(outcome: TerminalOutcome, retry: RetryPlan) -> TerminalReport {
        TerminalReport {
            outcome,
            final_counters: ProgressCounters {
                processed_items: 1,
                discovered_items: 1,
                total_items: Some(1),
                processed_bytes: 8,
                discovered_bytes: 8,
                total_bytes: Some(8),
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
            retry,
            change_hints: vec![FsChangeHint {
                directory: PathBuf::from(r"C:\target"),
            }],
        }
    }

    fn running(service: &mut OperationService) -> AttemptToken {
        let token = service.submit(request()).unwrap();
        assert!(matches!(
            service.start_next(),
            Some(ExecutorCommand::Prepare { .. })
        ));
        assert!(service.prepared(token));
        token
    }

    fn test_identity(value: &str) -> crate::filesystem_identity::FilesystemIdentity {
        crate::filesystem_identity::FilesystemIdentity::from_fallback_token(value.to_owned())
    }

    fn pending_move_request() -> OperationRequest {
        let mut request = request();
        request.kind = OperationKind::Move;
        request.move_identity_guard = Some(MoveIdentityGuard {

            source_identities: Vec::new(),
            destination_identity: test_identity("destination-at-submit"),
            });
        request
    }

    #[test]
    fn move_guard_resolution_is_one_shot_and_preserves_submit_destination_identity() {
        let mut service = OperationService::default();
        let token = service.submit(pending_move_request()).unwrap();
        let destination_identity = test_identity("destination-at-submit");
        let resolved = MoveIdentityGuard {

            source_identities: vec![test_identity("source-at-preflight")],
            destination_identity: destination_identity.clone(),
             };

        assert!(!service.resolve_move_identity_guard(token, resolved.clone()));
        assert!(matches!(
            service.start_next(),
            Some(ExecutorCommand::Prepare { .. })
        ));

        let wrong_destination = MoveIdentityGuard {
            destination_identity: test_identity("different-destination"),
            ..resolved.clone()
        };
        assert!(!service.resolve_move_identity_guard(token, wrong_destination));
        assert!(service.resolve_move_identity_guard(token, resolved.clone()));
        assert_eq!(
            service
                .job(token.job_id())
                .unwrap()
                .request
                .move_identity_guard,
            Some(resolved.clone())
        );

        let duplicate = MoveIdentityGuard {
            source_identities: vec![test_identity("late-different-source")],
            ..resolved
        };
        assert!(!service.resolve_move_identity_guard(token, duplicate));

        assert!(matches!(
            service.cancel(token),
            Some(ServiceEventOrCommand::Command(
                ExecutorCommand::Cancel { .. }
            ))
        ));
        assert!(!service.resolve_move_identity_guard(
            token,
            MoveIdentityGuard {

                source_identities: vec![test_identity("after-cancel")],
                destination_identity,
                                }
        ));
    }

    #[test]
    fn cancelled_pending_move_rejects_a_late_preflight_fact() {
        let mut service = OperationService::default();
        let token = service.submit(pending_move_request()).unwrap();
        assert!(matches!(
            service.start_next(),
            Some(ExecutorCommand::Prepare { .. })
        ));
        assert!(matches!(
            service.cancel(token),
            Some(ServiceEventOrCommand::Command(
                ExecutorCommand::Cancel { .. }
            ))
        ));

        assert!(!service.resolve_move_identity_guard(
            token,
            MoveIdentityGuard {

                source_identities: vec![test_identity("late-source")],
                destination_identity: test_identity("destination-at-submit"),
                                }
        ));
        assert!(
            service
                .job(token.job_id())
                .unwrap()
                .request
                .move_identity_guard
                .unwrap()
                .source_identities
                .is_empty()
        );
    }

    #[test]
    fn resolved_move_guard_survives_terminal_retry_unchanged() {
        let mut service = OperationService::default();
        let token = service.submit(pending_move_request()).unwrap();
        assert!(matches!(
            service.start_next(),
            Some(ExecutorCommand::Prepare { .. })
        ));
        let resolved = MoveIdentityGuard {

            source_identities: vec![test_identity("worker-observed-source")],
            destination_identity: test_identity("destination-at-submit"),
             };
        assert!(service.resolve_move_identity_guard(token, resolved.clone()));
        assert!(
            service
                .terminal(
                    token,
                    report(
                        TerminalOutcome::Partial,
                        RetryPlan::Explicit(vec![ItemId(0)])
                    ),
                )
                .is_some()
        );
        assert!(!service.resolve_move_identity_guard(
            token,
            MoveIdentityGuard {

                source_identities: vec![test_identity("after-terminal")],
                destination_identity: test_identity("destination-at-submit"),
                                }
        ));

        let retry = service.retry(token).unwrap();
        let Some(ExecutorCommand::Prepare { token, request, .. }) = service.start_next() else {
            panic!("retry should be queued for the executor")
        };
        assert_eq!(token, retry);
        assert_eq!(request.move_identity_guard, Some(resolved));
    }

    #[test]
    fn cancel_requested_then_success_preserves_the_receipt() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        assert!(matches!(
            service.cancel(token),
            Some(ServiceEventOrCommand::Command(
                ExecutorCommand::Cancel { .. }
            ))
        ));
        let terminal = report(TerminalOutcome::Succeeded, RetryPlan::NotAllowed);
        assert_eq!(
            service.terminal(token, terminal.clone()),
            Some(ServiceEvent::TerminalCommitted {
                token,
                report: terminal
            })
        );
        assert_eq!(
            service.job(token.job_id()).unwrap().attempts[0].state,
            AttemptState::Terminal
        );
    }

    #[test]
    fn terminal_retry_then_old_callbacks_cannot_touch_the_new_attempt() {
        let mut service = OperationService::default();
        let first = running(&mut service);
        assert!(
            service
                .terminal(
                    first,
                    report(
                        TerminalOutcome::Partial,
                        RetryPlan::Explicit(vec![ItemId(1)])
                    )
                )
                .is_some()
        );
        let second = service.retry(first).unwrap();
        assert_ne!(first.attempt_id(), second.attempt_id());
        assert!(!service.progress(
            first,
            ProgressUpdate {
                sequence: 1,
                counters: ProgressCounters::default(),
                current_item: None
            }
        ));
        assert!(service.start_next().is_some());
        assert!(service.prepared(second));
        assert!(service.progress(
            second,
            ProgressUpdate {
                sequence: 1,
                counters: ProgressCounters::default(),
                current_item: Some(ItemId(1))
            }
        ));
        assert!(
            service
                .terminal(
                    second,
                    report(
                        TerminalOutcome::Partial,
                        RetryPlan::Explicit(vec![ItemId(2)])
                    )
                )
                .is_some()
        );

        assert_eq!(service.retry(first), Err(RetryError::NotRetryable));
        let job = service.job(first.job_id()).unwrap();
        assert_eq!(
            job.attempts.len(),
            2,
            "stale retry does not create attempt C"
        );
        assert_eq!(job.attempts.last().unwrap().token, second);

        let third = service.retry(second).unwrap();
        assert_ne!(third.attempt_id(), second.attempt_id());
        assert_eq!(service.job(first.job_id()).unwrap().attempts.len(), 3);
    }

    #[test]
    fn held_attempt_accepts_progress_only_after_claim_including_preparing() {
        let mut service = OperationService::default();
        let token = service.submit_held(request()).unwrap();
        let update = ProgressUpdate {
            sequence: 1,
            counters: ProgressCounters::default(),
            current_item: Some(ItemId(0)),
        };

        assert!(!service.progress(token, update.clone()));
        assert!(service.release_held(token));
        assert!(!service.progress(token, update.clone()));
        assert!(matches!(
            service.start_next(),
            Some(ExecutorCommand::Prepare { token: started, .. }) if started == token
        ));
        assert_eq!(
            service.job(token.job_id()).unwrap().attempts[0].state,
            AttemptState::Preparing
        );
        assert!(service.progress(token, update));
    }

    #[test]
    fn jobs_projection_is_ordered_and_includes_terminal_history() {
        let mut service = OperationService::default();
        let first = running(&mut service);
        let second = service.submit(request()).unwrap();
        assert!(
            service
                .terminal(
                    first,
                    report(TerminalOutcome::Succeeded, RetryPlan::NotAllowed)
                )
                .is_some()
        );

        let jobs = service.jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, first.job_id());
        assert_eq!(jobs[1].id, second.job_id());
        assert_eq!(jobs[0].attempts[0].state, AttemptState::Terminal);
        assert_eq!(jobs[1].attempts[0].state, AttemptState::Queued);
    }

    #[test]
    fn recent_jobs_projection_is_bounded_and_newest_first() {
        let mut service = OperationService::default();
        let tokens = (0..8)
            .map(|_| service.submit(request()).unwrap())
            .collect::<Vec<_>>();

        let recent = service.recent_jobs(3);
        assert_eq!(
            recent.iter().map(|job| job.id).collect::<Vec<_>>(),
            tokens[5..]
                .iter()
                .rev()
                .map(|token| token.job_id())
                .collect::<Vec<_>>()
        );
        assert!(service.recent_jobs(0).is_empty());
    }

    #[test]
    fn cancel_invalidates_a_stale_conflict_answer() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let conflict = service
            .conflict(
                token,
                ConflictNotice {
                    item: ItemId(0),
                    probe_revision: 1,
                    description: "exists".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            )
            .unwrap();
        assert!(service.cancel(token).is_some());
        assert_eq!(
            service.resolve_conflict(conflict, ConflictDecision::Replace),
            None
        );
    }

    #[test]
    fn unsupported_replace_does_not_consume_the_pending_conflict() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let conflict = service
            .conflict(
                token,
                ConflictNotice {
                    item: ItemId(0),
                    probe_revision: 1,
                    description: "directory exists".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            )
            .unwrap();
        assert_eq!(
            service.resolve_conflict(conflict, ConflictDecision::Replace),
            None
        );
        assert!(matches!(
            service.resolve_conflict(conflict, ConflictDecision::Skip),
            Some(ExecutorCommand::ResolveConflict {
                decision: ConflictDecision::Skip,
                ..
            })
        ));
    }

    #[test]
    fn stale_conflict_close_cannot_cancel_a_newer_conflict() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let first = service
            .conflict(
                token,
                ConflictNotice {
                    item: ItemId(0),
                    probe_revision: 1,
                    description: "first".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            )
            .unwrap();
        assert!(
            service
                .resolve_conflict(first, ConflictDecision::Skip)
                .is_some()
        );
        let second = service
            .conflict(
                token,
                ConflictNotice {
                    item: ItemId(0),
                    probe_revision: 2,
                    description: "second".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            )
            .unwrap();
        assert_eq!(service.cancel_conflict(first), None);
        assert!(matches!(
            service.cancel_conflict(second),
            Some(ExecutorCommand::Cancel { .. })
        ));
    }

    #[test]
    fn progress_is_monotonic_and_terminal_is_final() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let later = ProgressUpdate {
            sequence: 10,
            counters: ProgressCounters {
                processed_items: 10,
                ..Default::default()
            },
            current_item: Some(ItemId(10)),
        };
        assert!(service.progress(token, later.clone()));
        assert!(!service.progress(
            token,
            ProgressUpdate {
                sequence: 8,
                counters: ProgressCounters::default(),
                current_item: None
            }
        ));
        assert!(
            service
                .terminal(
                    token,
                    report(TerminalOutcome::Succeeded, RetryPlan::NotAllowed)
                )
                .is_some()
        );
        assert!(!service.progress(
            token,
            ProgressUpdate {
                sequence: 11,
                counters: ProgressCounters::default(),
                current_item: None
            }
        ));
    }

    #[test]
    fn progress_is_rejected_after_cancellation_is_requested() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let update = ProgressUpdate {
            sequence: 1,
            counters: ProgressCounters::default(),
            current_item: Some(ItemId(0)),
        };

        assert!(service.cancel(token).is_some());
        assert_eq!(
            service.job(token.job_id()).unwrap().attempts[0].state,
            AttemptState::CancelRequested
        );
        assert!(!service.progress(token, update));
    }

    #[test]
    fn conflict_pause_rejects_progress_until_exact_resolution_resumes_running() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let conflict = service
            .conflict(
                token,
                ConflictNotice {
                    item: ItemId(0),
                    probe_revision: 1,
                    description: "exists".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            )
            .unwrap();
        let update = ProgressUpdate {
            sequence: 1,
            counters: ProgressCounters::default(),
            current_item: Some(ItemId(0)),
        };

        assert!(!service.progress(token, update.clone()));
        assert!(matches!(
            service.resolve_conflict(conflict, ConflictDecision::Skip),
            Some(ExecutorCommand::ResolveConflict {
                token: resolved,
                decision: ConflictDecision::Skip,
            }) if resolved == conflict
        ));
        assert_eq!(
            service.job(token.job_id()).unwrap().attempts[0].state,
            AttemptState::Running
        );
        assert!(service.progress(token, update));
    }

    #[test]
    fn cancel_before_start_is_terminal_without_an_executor_command() {
        let mut service = OperationService::default();
        let token = service.submit(request()).unwrap();
        assert!(matches!(
            service.cancel(token),
            Some(ServiceEventOrCommand::Event(_))
        ));
        assert_eq!(service.start_next(), None);
    }

    #[test]
    fn queued_multi_source_zip_cancellation_commits_one_cancelled_receipt() {
        let parent = tempfile::tempdir().unwrap();
        let first = parent.path().join("first.txt");
        let second = parent.path().join("second.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let output = parent.path().join("first.zip");
        let first_guard = test_zip_create_guard(&first, &output);
        let second_guard = test_zip_create_guard(&second, &output);
        let zip_request = OperationRequest::create_zip_many(
            vec![first.clone(), second],
            output.clone(),
            ZipArchiveCreateGuard {
                destination_parent_identity: first_guard.destination_parent_identity.clone(),
                sources: vec![first_guard, second_guard]
                    .into_iter()
                    .map(|guard| ZipCreateSourceGuard {
                        source_identity: guard.source_identity,
                        source_kind: guard.source_kind,
                        entry_name: guard.entry_name,
                    })
                    .collect(),
            },
        )
        .unwrap();
        let mut service = OperationService::default();
        let occupied = service.submit(request()).unwrap();
        assert!(
            matches!(service.start_next(), Some(ExecutorCommand::Prepare { token, .. }) if token == occupied)
        );
        assert!(service.prepared(occupied));

        let zip_token = service.submit(zip_request).unwrap();
        let Some(ServiceEventOrCommand::Event(ServiceEvent::TerminalCommitted { token, report })) =
            service.cancel(zip_token)
        else {
            panic!("queued ZIP cancellation commits directly without starting a worker");
        };
        assert_eq!(token, zip_token);
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert_eq!(report.final_counters.total_items, Some(1));
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(report.receipts[0].item, ItemId(0));
        assert_eq!(
            report.receipts[0].outcome,
            ItemOutcome::CancelledBeforeStart
        );
        assert_eq!(service.start_next(), None);
        assert!(
            service.terminal(zip_token, report.clone()).is_none(),
            "late worker completion cannot revive the cancelled attempt"
        );
        assert!(!output.exists());
    }

    #[test]
    fn terminal_is_accepted_once_and_origin_does_not_own_the_job() {
        let mut service = OperationService::default();
        let token = running(&mut service);
        let terminal = report(TerminalOutcome::Succeeded, RetryPlan::NotAllowed);
        assert!(service.terminal(token, terminal.clone()).is_some());
        assert_eq!(
            service.terminal(
                token,
                report(TerminalOutcome::Failed, RetryPlan::NotAllowed)
            ),
            None
        );
        let snapshot = service.job(token.job_id()).unwrap();
        assert_eq!(snapshot.request.origin.unwrap().view_id, 4);
        assert_eq!(snapshot.attempts[0].terminal, Some(terminal));
    }

    #[test]
    fn cross_volume_move_retry_signature_requires_nonzero_change_time_and_single_link() {
        assert!(!SourceCleanupLeafSignature { number_of_links: 1, change_time: 0 }
            .is_reliable_for_cross_volume_move_retry());
        assert!(!SourceCleanupLeafSignature { number_of_links: 2, change_time: 1 }
            .is_reliable_for_cross_volume_move_retry());
        assert!(SourceCleanupLeafSignature { number_of_links: 1, change_time: 1 }
            .is_reliable_for_cross_volume_move_retry());
    }

    #[test]
    fn cross_volume_move_zero_change_time_proof_is_not_recorded() {
        let mut request = request();
        request.kind = OperationKind::Move;
        let mut service = OperationService::default();
        let token = service.submit(request).unwrap();
        service.start_next();
        assert!(service.prepared(token));
        let proof = |change_time| CrossVolumeMoveNoEffectProof {
            item: ItemId(0),
            source: PathBuf::from(r"C:\source.txt"),
            destination: PathBuf::from(r"C:\target\source.txt"),
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            source_signature: SourceCleanupLeafSignature { number_of_links: 1, change_time },
            destination_parent_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(3, 4),
        };
        assert!(!service.record_cross_volume_move_no_effect_proof(token, proof(0)));
        assert!(service.record_cross_volume_move_no_effect_proof(token, proof(10)));
    }

    #[test]
    fn progress_rows_retain_old_explicit_retry_but_exclude_success_cancel_and_guarded_retry() {
        let mut service = OperationService::default();
        let retry_items = RetryPlan::Explicit(vec![ItemId(0)]);
        let old_failure = running(&mut service);
        service.terminal(
            old_failure,
            report(TerminalOutcome::Failed, retry_items.clone()),
        );

        for retry in [RetryPlan::RequiresReinspection, RetryPlan::NotAllowed] {
            let token = running(&mut service);
            service.terminal(token, report(TerminalOutcome::Failed, retry));
        }
        let succeeded = running(&mut service);
        service.terminal(
            succeeded,
            report(TerminalOutcome::Succeeded, retry_items.clone()),
        );
        let cancelled = running(&mut service);
        service.terminal(
            cancelled,
            report(TerminalOutcome::Cancelled, retry_items),
        );

        for _ in 0..6 {
            let newer = running(&mut service);
            service.terminal(
                newer,
                report(TerminalOutcome::Succeeded, RetryPlan::NotAllowed),
            );
        }

        assert_eq!(service.recent_jobs(6).len(), 6);
        assert!(!service.recent_jobs(6).iter().any(|job| job.attempts[0].token == old_failure));
        let rows = service.progress_rows();
        let old_row = rows.iter().find(|row| row.token == old_failure).unwrap();
        assert!(old_row.retryable);
        assert_eq!(old_row.terminal_outcome, Some(TerminalOutcome::Failed));
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().filter(|row| row.token != old_failure).all(|row| !row.retryable));
    }
}
