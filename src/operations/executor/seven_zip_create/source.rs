use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::operations::{
    executor::{CopyError, ExecutorFact as Fact, WorkerControl},
    journal::{JournalPath, JournalSevenZipHandleCreate as Ledger},
    model::{
        AttemptToken, ItemId, ItemOutcome, ItemReceipt, OperationRequest, ProgressCounters,
        RetryPlan, TerminalOutcome, TerminalReport,
    },
};

const WORK_BUDGET: Duration = Duration::from_secs(60 * 60);
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerCheckpoint {
    BeforeCandidateCreate,
    BeforeStageCreate,
    BeforeVerify,
    BeforePublishIntent,
    AfterPublishIntent,
    AfterPublishMutation,
}

pub(super) fn execute(
    token: AttemptToken,
    request: &OperationRequest,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) {
    let mut checkpoint = |_: WorkerCheckpoint, _: &mut Instant, _: &AtomicBool| {};
    let mut copy_chunk = |_: &File, _: &File, _: u64| {};
    execute_with_image(
        token,
        request,
        provider,
        crate::archive_provider::application_image::lease_for_operation(),
        cancelled,
        controls,
        emit,
        &mut checkpoint,
        &mut copy_chunk,
    );
}

#[cfg(test)]
pub(super) fn execute_with_image_for_test(
    token: AttemptToken,
    request: &OperationRequest,
    provider: crate::archive_provider::ArchiveProviderOwner,
    image: crate::archive_provider::application_image::ApplicationImageLease,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) {
    let mut checkpoint = |_: WorkerCheckpoint, _: &mut Instant, _: &AtomicBool| {};
    let mut copy_chunk = |_: &File, _: &File, _: u64| {};
    execute_with_image_for_test_hooks(
        token,
        request,
        provider,
        image,
        cancelled,
        controls,
        emit,
        &mut checkpoint,
        &mut copy_chunk,
    );
}

#[cfg(test)]
fn execute_with_image_for_test_hooks(
    token: AttemptToken,
    request: &OperationRequest,
    provider: crate::archive_provider::ArchiveProviderOwner,
    image: crate::archive_provider::application_image::ApplicationImageLease,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
    checkpoint: &mut impl FnMut(WorkerCheckpoint, &mut Instant, &AtomicBool),
    copy_chunk: &mut dyn FnMut(&File, &File, u64),
) {
    execute_with_image(
        token,
        request,
        Some(provider),
        Some(image),
        cancelled,
        controls,
        emit,
        checkpoint,
        copy_chunk,
    );
}

fn execute_with_image(
    token: AttemptToken,
    request: &OperationRequest,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    image: Option<crate::archive_provider::application_image::ApplicationImageLease>,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
    checkpoint: &mut impl FnMut(WorkerCheckpoint, &mut Instant, &AtomicBool),
    copy_chunk: &mut dyn FnMut(&File, &File, u64),
) {
    // provider discovery の結果だけでは実行権限にならず、startup capability がなければ I/O 前に止める。
    let result = match provider {
        Some(provider) => match image {
            Some(image) => execute_inner(
                token, request, &provider, image, cancelled, controls, emit, checkpoint, copy_chunk,
            ),
            None => Err(WorkerError::Rejected(
                "application image capability is unavailable",
            )),
        },
        None => Err(WorkerError::Rejected(
            "7-Zip provider was not captured by the application",
        )),
    };
    let (outcome, item_outcome, output, detail, retry) = match result {
        Ok(path) => (
            TerminalOutcome::Succeeded,
            ItemOutcome::Completed,
            Some(path),
            None,
            RetryPlan::NotAllowed,
        ),
        Err(WorkerError::Cancelled) => (
            TerminalOutcome::Cancelled,
            ItemOutcome::CancelledBeforeStart,
            None,
            Some("7z creation was cancelled".to_owned()),
            RetryPlan::NotAllowed,
        ),
        Err(WorkerError::Unknown(detail)) => (
            TerminalOutcome::Unknown,
            ItemOutcome::Unknown,
            None,
            Some(detail),
            RetryPlan::RequiresReinspection,
        ),
        Err(WorkerError::Rejected(detail)) => (
            TerminalOutcome::Failed,
            ItemOutcome::Failed,
            None,
            Some(detail.to_owned()),
            RetryPlan::RequiresReinspection,
        ),
    };
    let destination = request.destination.as_deref();
    emit(Fact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: ProgressCounters {
                total_items: Some(1),
                processed_items: u64::from(item_outcome == ItemOutcome::Completed),
                ..ProgressCounters::default()
            },
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
            change_hints: destination
                .and_then(Path::parent)
                .map(|directory| {
                    vec![crate::operations::model::FsChangeHint {
                        directory: directory.to_path_buf(),
                    }]
                })
                .unwrap_or_default(),
        },
    });
}

#[derive(Debug)]
enum WorkerError {
    Cancelled,
    Rejected(&'static str),
    Unknown(String),
}

fn execute_inner(
    token: AttemptToken,
    request: &OperationRequest,
    provider: &crate::archive_provider::ArchiveProviderOwner,
    image_lease: crate::archive_provider::application_image::ApplicationImageLease,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
    checkpoint: &mut impl FnMut(WorkerCheckpoint, &mut Instant, &AtomicBool),
    copy_chunk: &mut dyn FnMut(&File, &File, u64),
) -> Result<PathBuf, WorkerError> {
    use crate::operations::journal::{
        JournalSevenZipHandleEntry, JournalSevenZipHandleIdentity, JournalSevenZipHandleObject,
        JournalSevenZipHandleObjectState as ObjectState, JournalSevenZipHandlePhase as Phase,
        JournalSevenZipHandleSource,
    };
    use crate::operations::model::OperationKind;

    if request.kind != OperationKind::Create7z || !request.seven_zip_create_shape_is_valid() {
        return Err(WorkerError::Rejected(
            "captured Create7z request is invalid",
        ));
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkerError::Cancelled);
    }
    let mut deadline = Instant::now() + WORK_BUDGET;
    let destination = request
        .destination
        .as_deref()
        .ok_or(WorkerError::Rejected("7z destination is missing"))?;
    let guard = request
        .seven_zip_create_guard
        .as_ref()
        .ok_or(WorkerError::Rejected("7z guard is missing"))?;
    let _provider_lease_before_capture = provider
        .lease_7z_creation(guard.provider_revision, &guard.provider_binding)
        .ok_or(WorkerError::Rejected(
            "captured 7z provider binding is stale",
        ))?;
    let parent_path = destination
        .parent()
        .ok_or(WorkerError::Rejected("destination parent is missing"))?;
    super::super::windows::ensure_ntfs_archive_volume(parent_path)
        .map_err(|_| WorkerError::Rejected("destination volume is not NTFS"))?;
    let parent_route = super::super::windows::lock_virtual_destination(
        parent_path,
        Some(&guard.destination_parent_identity),
    )
    .map_err(|_| WorkerError::Rejected("destination route or identity changed"))?;
    let parent = parent_route
        .last()
        .ok_or(WorkerError::Rejected("destination route is empty"))?;
    let parent_identity = super::super::windows::directory_identity_text(parent)
        .map_err(|_| WorkerError::Rejected("destination identity could not be read"))?;
    if identity_text(&guard.destination_parent_identity) != parent_identity {
        return Err(WorkerError::Rejected("destination parent identity changed"));
    }

    let mut pins = Vec::<SourcePin>::new();
    let mut journal_sources = Vec::<JournalSevenZipHandleSource>::new();
    let mut journal_manifest = Vec::<JournalSevenZipHandleEntry>::new();
    let mut verify_manifest = Vec::<super::verify::ManifestEntry>::new();
    let mut entry_specs = Vec::<(String, bool, u64, Option<(usize, Option<usize>)>)>::new();
    let mut payload = 0u64;
    let mut name_bytes = 0usize;
    for (index, (path, source_guard)) in request.sources.iter().zip(&guard.sources).enumerate() {
        check_work(deadline, cancelled)?;
        if path.parent() != Some(parent_path) {
            return Err(WorkerError::Rejected(
                "7z roots are not destination siblings",
            ));
        }
        let (volume, file_index) =
            source_guard
                .source_identity
                .windows_parts()
                .ok_or(WorkerError::Rejected(
                    "source identity is outside the NTFS identity domain",
                ))?;
        let root_identity = format!("{volume}:{file_index:016x}");
        let root_name = source_guard.entry_name.clone();
        name_bytes = name_bytes
            .checked_add(root_name.len())
            .ok_or(WorkerError::Rejected("7z name bound overflow"))?;
        if journal_manifest.len() >= 1024 || name_bytes > 2 * 1024 * 1024 {
            return Err(WorkerError::Rejected("7z capture bounds exceeded"));
        }
        let root_identity_dto = JournalSevenZipHandleIdentity { volume, file_index };
        match source_guard.source_kind {
            crate::directory::EntryKind::File => {
                let file = super::super::windows::open_pinned_file_with_access(path, false)
                    .map_err(|_| {
                        WorkerError::Rejected("source file route is not an ordinary NTFS file")
                    })?;
                let actual = super::super::windows::regular_file_identity(&file)
                    .map_err(|_| WorkerError::Rejected("source identity could not be checked"))?;
                if actual.volume != volume
                    || ((u64::from(actual.index_high) << 32) | u64::from(actual.index_low))
                        != file_index
                {
                    return Err(WorkerError::Rejected("source identity changed"));
                }
                let (length, digest) = hash_file(&file, deadline, cancelled)?;
                payload = payload
                    .checked_add(length)
                    .ok_or(WorkerError::Rejected("captured payload size overflow"))?;
                let digest_text = hex(&digest);
                journal_sources.push(JournalSevenZipHandleSource {
                    path: JournalPath::from_path(path),
                    identity: root_identity_dto.clone(),
                    entry_name: root_name.clone(),
                    directory: false,
                    length,
                    sha256: Some(digest_text.clone()),
                });
                journal_manifest.push(JournalSevenZipHandleEntry {
                    name: root_name.clone(),
                    directory: false,
                    source_index: Some(index as u32),
                    length,
                    source_identity: Some(root_identity_dto),
                    sha256: Some(digest_text),
                });
                verify_manifest.push(super::verify::ManifestEntry {
                    name: root_name.clone(),
                    kind: super::verify::EntryKind::File,
                    length,
                    sha256: digest,
                });
                entry_specs.push((root_name, false, length, Some((pins.len(), None))));
                pins.push(SourcePin::File(file));
            }
            crate::directory::EntryKind::Folder => {
                let tree = super::super::windows::preflight_pinned_source_tree_for_7z(
                    path,
                    cancelled,
                    &parent_identity,
                    &root_identity,
                    super::super::windows::SourceTreeLimits {
                        entries: 1023usize.saturating_sub(journal_manifest.len()),
                        depth: 127,
                        relative_name_bytes: (2 * 1024 * 1024usize).saturating_sub(name_bytes),
                        record_bytes: 3 * 1024 * 1024,
                        emitted_name_prefix_bytes: root_name.len(),
                        zip_utf8_name_bytes: true,
                    },
                    deadline,
                )
                .map_err(|error| {
                    if matches!(error, CopyError::Cancelled) {
                        if cancelled.load(Ordering::Acquire) {
                            WorkerError::Cancelled
                        } else {
                            WorkerError::Rejected("7z work budget expired")
                        }
                    } else {
                        WorkerError::Rejected("source tree is not a bounded ordinary NTFS tree")
                    }
                })?;
                journal_sources.push(JournalSevenZipHandleSource {
                    path: JournalPath::from_path(path),
                    identity: root_identity_dto.clone(),
                    entry_name: root_name.clone(),
                    directory: true,
                    length: 0,
                    sha256: None,
                });
                journal_manifest.push(JournalSevenZipHandleEntry {
                    name: root_name.clone(),
                    directory: true,
                    source_index: Some(index as u32),
                    length: 0,
                    source_identity: None,
                    sha256: None,
                });
                verify_manifest.push(super::verify::ManifestEntry {
                    name: root_name.clone(),
                    kind: super::verify::EntryKind::Directory,
                    length: 0,
                    sha256: [0; 32],
                });
                let pin_index = pins.len();
                entry_specs.push((root_name.clone(), true, 0, None));
                for entry in &tree.entries {
                    let mut name = root_name.clone();
                    for component in &entry.relative_components {
                        name.push('/');
                        name.push_str(
                            &String::from_utf16(component)
                                .map_err(|_| WorkerError::Rejected("source name is not Unicode"))?,
                        );
                    }
                    name_bytes = name_bytes
                        .checked_add(name.len())
                        .ok_or(WorkerError::Rejected("7z name bound overflow"))?;
                    if name_bytes > 2 * 1024 * 1024 || journal_manifest.len() >= 1024 {
                        return Err(WorkerError::Rejected("7z capture bounds exceeded"));
                    }
                    let directory = entry.node_kind
                        == crate::operations::journal::JournalDirectoryNodeKind::Directory;
                    let (length, digest, identity, file_index) = if directory {
                        (0, None, None, None)
                    } else {
                        let Some(Some(super::super::windows::PinnedSourceHandle::File(file))) =
                            tree._handles.get(entry.handle_index)
                        else {
                            return Err(WorkerError::Rejected(
                                "pinned source tree handle is missing",
                            ));
                        };
                        let (length, digest) = hash_file(file, deadline, cancelled)?;
                        payload = payload
                            .checked_add(length)
                            .ok_or(WorkerError::Rejected("captured payload size overflow"))?;
                        let id =
                            super::super::windows::regular_file_identity(file).map_err(|_| {
                                WorkerError::Rejected("source identity could not be checked")
                            })?;
                        let identity = JournalSevenZipHandleIdentity {
                            volume: id.volume,
                            file_index: (u64::from(id.index_high) << 32) | u64::from(id.index_low),
                        };
                        (
                            length,
                            Some(digest),
                            Some(identity),
                            Some((entry.handle_index, length)),
                        )
                    };
                    let digest_string = digest.as_ref().map(|value| hex(value));
                    journal_manifest.push(JournalSevenZipHandleEntry {
                        name: name.clone(),
                        directory,
                        source_index: Some(index as u32),
                        length,
                        source_identity: identity,
                        sha256: digest_string,
                    });
                    verify_manifest.push(super::verify::ManifestEntry {
                        name: name.clone(),
                        kind: if directory {
                            super::verify::EntryKind::Directory
                        } else {
                            super::verify::EntryKind::File
                        },
                        length,
                        sha256: digest.unwrap_or([0; 32]),
                    });
                    entry_specs.push((
                        name,
                        directory,
                        length,
                        file_index.map(|(handle, _)| (pin_index, Some(handle))),
                    ));
                }
                pins.push(SourcePin::Tree(tree));
            }
        }
    }
    if journal_manifest.is_empty() || journal_manifest.len() > 1024 || name_bytes > 2 * 1024 * 1024
    {
        return Err(WorkerError::Rejected("7z capture bounds exceeded"));
    }
    let provider_lease = provider
        .lease_7z_creation(guard.provider_revision, &guard.provider_binding)
        .ok_or(WorkerError::Rejected(
            "7z provider changed during source capture",
        ))?;
    let maximum = payload
        .checked_mul(2)
        .and_then(|value| value.checked_add(8 * 1024 * 1024 + 32))
        .ok_or(WorkerError::Rejected("archive size bound overflow"))?;
    let candidate_path = temporary_sibling(destination, token, "candidate");
    let stage_path = temporary_sibling(destination, token, "stage");
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkerError::Cancelled);
    }
    let binding = &guard.provider_binding;
    let mut ledger = Ledger {
        sources: journal_sources,
        provider_revision: guard.provider_revision,
        executable: JournalPath::from_path(&binding.executable.path),
        executable_identity: format!("{:?}", binding.executable.identity),
        seven_zip_dll: JournalPath::from_path(&binding.zip_dll.path),
        seven_zip_dll_identity: format!("{:?}", binding.zip_dll.identity),
        output: JournalPath::from_path(destination),
        output_parent_identity: format!("{:?}", guard.destination_parent_identity),
        manifest: journal_manifest,
        candidate: JournalSevenZipHandleObject {
            path: JournalPath::from_path(&candidate_path),
            state: ObjectState::CreateIntent,
            identity: None,
            length: None,
        },
        stage: JournalSevenZipHandleObject {
            path: JournalPath::from_path(&stage_path),
            state: ObjectState::Planned,
            identity: None,
            length: None,
        },
        phase: Phase::CandidateCreateIntent,
        aborting: false,
        sequence: 1,
        archive_identity: None,
    };
    ledger
        .validate()
        .map_err(|_| WorkerError::Rejected("7z capture metadata failed ledger validation"))?;
    let create_permitted = journal(token, &ledger, controls, emit)?;
    if !create_permitted || cancelled.load(Ordering::Acquire) {
        if !cancelled.load(Ordering::Acquire) {
            return Err(WorkerError::Unknown(
                "candidate create intent has no mutation authority".into(),
            ));
        }
        ledger.aborting = true;
        ledger.candidate.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    if let Err(stop) = check_work(deadline, cancelled) {
        ledger.aborting = true;
        ledger.candidate.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(stop);
    }
    checkpoint(
        WorkerCheckpoint::BeforeCandidateCreate,
        &mut deadline,
        cancelled,
    );
    if let Err(stop) = check_work(deadline, cancelled) {
        ledger.aborting = true;
        ledger.candidate.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(stop);
    }
    let mut candidate = match super::super::windows::create_7z_staged_file(
        parent,
        &candidate_path
            .file_name()
            .ok_or(WorkerError::Rejected("candidate name missing"))?
            .encode_wide()
            .collect::<Vec<_>>(),
    ) {
        Ok(file) => file,
        Err(CopyError::DestinationExists) => {
            // CREATE_NEW が衝突したとき、その名前のファイルはこの処理の所有物ではない。
            ledger.aborting = true;
            ledger.candidate.state = ObjectState::CreateFailed;
            ledger.sequence = ledger
                .sequence
                .checked_add(1)
                .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
            journal(token, &ledger, controls, emit)?;
            ledger.stage.state = ObjectState::NeverCreated;
            ledger.sequence = ledger
                .sequence
                .checked_add(1)
                .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
            journal(token, &ledger, controls, emit)?;
            ledger.phase = Phase::Discarded;
            ledger.sequence = ledger
                .sequence
                .checked_add(1)
                .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
            journal(token, &ledger, controls, emit)?;
            return Err(WorkerError::Rejected("candidate name collision"));
        }
        Err(_) => {
            return Err(WorkerError::Unknown(
                "candidate creation result is uncertain; reinspection is required".into(),
            ));
        }
    };
    let candidate_identity = file_identity(&candidate)?;
    ledger.candidate.state = ObjectState::Owned;
    ledger.candidate.identity = Some(candidate_identity.clone());
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let helper_permitted = journal(token, &ledger, controls, emit)?;
    if !helper_permitted || cancelled.load(Ordering::Acquire) {
        if !cancelled.load(Ordering::Acquire) {
            return Err(WorkerError::Unknown(
                "candidate ownership has no helper launch authority".into(),
            ));
        }
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
            candidate,
            controls,
            emit,
        )?;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    let image = image_lease;
    let entries = build_entries(&entry_specs, &pins)?;
    let Some(launch) = crate::archive_provider::build_seven_zip_helper_launch(
        provider_lease,
        image,
        &candidate,
        &entries,
    ) else {
        return discard_candidate_before_stage(
            token,
            &mut ledger,
            candidate,
            WorkerError::Rejected("7z helper launch admission failed"),
            controls,
            emit,
        );
    };
    let helper = crate::archive_provider::run_seven_zip_helper_launch(
        launch,
        parent_path,
        deadline,
        cancelled,
    );
    match helper {
        crate::archive_provider::SevenZipHelperRunOutcome::Exited { code: 0 } => {}
        crate::archive_provider::SevenZipHelperRunOutcome::Cancelled => {
            return discard_candidate_before_stage(
                token,
                &mut ledger,
                candidate,
                WorkerError::Cancelled,
                controls,
                emit,
            );
        }
        crate::archive_provider::SevenZipHelperRunOutcome::Exited { .. }
        | crate::archive_provider::SevenZipHelperRunOutcome::TimedOut
        | crate::archive_provider::SevenZipHelperRunOutcome::LaunchFailed
        | crate::archive_provider::SevenZipHelperRunOutcome::BindingRejected => {
            return discard_candidate_before_stage(
                token,
                &mut ledger,
                candidate,
                WorkerError::Rejected("7z helper failed before stage creation"),
                controls,
                emit,
            );
        }
    }
    let candidate_len = match candidate.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => {
            return discard_candidate_before_stage(
                token,
                &mut ledger,
                candidate,
                WorkerError::Unknown("candidate length could not be read".into()),
                controls,
                emit,
            );
        }
    };
    if candidate_len > maximum {
        return discard_candidate_before_stage(
            token,
            &mut ledger,
            candidate,
            WorkerError::Rejected("candidate exceeded the captured-size bound"),
            controls,
            emit,
        );
    }
    ledger.candidate.length = Some(candidate_len);
    ledger.phase = Phase::CandidateReady;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let candidate_ready_permitted = journal(token, &ledger, controls, emit)?;
    if !candidate_ready_permitted || cancelled.load(Ordering::Acquire) {
        if !cancelled.load(Ordering::Acquire) {
            return Err(WorkerError::Unknown(
                "candidate-ready receipt has no stage creation authority".into(),
            ));
        }
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
            candidate,
            controls,
            emit,
        )?;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    ledger.phase = Phase::StageCreateIntent;
    ledger.stage.state = ObjectState::CreateIntent;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let stage_create_permitted = journal(token, &ledger, controls, emit)?;
    if !stage_create_permitted || cancelled.load(Ordering::Acquire) {
        if !cancelled.load(Ordering::Acquire) {
            return Err(WorkerError::Unknown(
                "stage create intent has no creation authority".into(),
            ));
        }
        ledger.aborting = true;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
            candidate,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    if let Err(stop) = check_work(deadline, cancelled) {
        ledger.aborting = true;
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
            candidate,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(stop);
    }
    checkpoint(
        WorkerCheckpoint::BeforeStageCreate,
        &mut deadline,
        cancelled,
    );
    let mut stage = match super::super::windows::create_7z_staged_file(
        parent,
        &stage_path
            .file_name()
            .ok_or(WorkerError::Rejected("stage name missing"))?
            .encode_wide()
            .collect::<Vec<_>>(),
    ) {
        Ok(file) => file,
        Err(CopyError::DestinationExists) => {
            ledger.aborting = true;
            ledger.stage.state = ObjectState::CreateFailed;
            ledger.sequence = ledger
                .sequence
                .checked_add(1)
                .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
            journal(token, &ledger, controls, emit)?;
            ledger.aborting = true;
            cleanup_one(
                token,
                &mut ledger,
                crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
                candidate,
                controls,
                emit,
            )?;
            ledger.phase = Phase::Discarded;
            ledger.sequence = ledger
                .sequence
                .checked_add(1)
                .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
            journal(token, &ledger, controls, emit)?;
            return Err(WorkerError::Rejected("stage name collision"));
        }
        Err(_) => {
            return Err(WorkerError::Unknown(
                "stage creation result is uncertain; reinspection is required".into(),
            ));
        }
    };
    ledger.stage.state = ObjectState::Owned;
    ledger.stage.identity = Some(file_identity(&stage)?);
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let stage_owned_permitted = journal(token, &ledger, controls, emit)?;
    if !stage_owned_permitted || cancelled.load(Ordering::Acquire) {
        if !cancelled.load(Ordering::Acquire) {
            return Err(WorkerError::Unknown(
                "stage ownership has no copy authority".into(),
            ));
        }
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
            candidate,
            controls,
            emit,
        )?;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Stage,
            stage,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    let staged = copy_exact(
        &mut candidate,
        &mut stage,
        candidate_len,
        deadline,
        cancelled,
        copy_chunk,
    )
    .and_then(|()| {
        check_work(deadline, cancelled)?;
        checkpoint(WorkerCheckpoint::BeforeVerify, &mut deadline, cancelled);
        super::verify::verify(&mut stage, &verify_manifest, deadline, cancelled).map_err(|error| {
            match error {
                super::verify::VerifyError::Cancelled => WorkerError::Cancelled,
                super::verify::VerifyError::Deadline => {
                    WorkerError::Rejected("7z work budget expired")
                }
                _ => WorkerError::Unknown("retained stage verification failed".into()),
            }
        })
    })
    .and_then(|()| {
        check_work(deadline, cancelled)?;
        super::super::windows::flush_staged_file(&stage)
            .map_err(|_| WorkerError::Unknown("stage flush failed".into()))
    });
    if let Err(error) = staged {
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
            candidate,
            controls,
            emit,
        )?;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Stage,
            stage,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(error);
    }
    ledger.stage.length = Some(candidate_len);
    ledger.phase = Phase::StageReady;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let stage_ready_permitted = journal(token, &ledger, controls, emit)?;
    if !stage_ready_permitted && !cancelled.load(Ordering::Acquire) {
        return Err(WorkerError::Unknown(
            "verified stage receipt has no deletion authority".into(),
        ));
    }
    ledger.aborting = cancelled.load(Ordering::Acquire);
    cleanup_one(
        token,
        &mut ledger,
        crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
        candidate,
        controls,
        emit,
    )?;
    ledger.phase = Phase::CandidateCleaned;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let candidate_cleaned_permitted = journal(token, &ledger, controls, emit)?;
    if !candidate_cleaned_permitted && !cancelled.load(Ordering::Acquire) {
        return Err(WorkerError::Unknown(
            "candidate cleanup receipt has no publication authority".into(),
        ));
    }
    checkpoint(
        WorkerCheckpoint::BeforePublishIntent,
        &mut deadline,
        cancelled,
    );
    if cancelled.load(Ordering::Acquire) {
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Stage,
            stage,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    ledger.phase = Phase::PublishIntent;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    let publish_permitted = journal(token, &ledger, controls, emit)?;
    if !publish_permitted || cancelled.load(Ordering::Acquire) {
        if !cancelled.load(Ordering::Acquire) {
            return Err(WorkerError::Unknown(
                "publish intent has no rename authority".into(),
            ));
        }
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Stage,
            stage,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(WorkerError::Cancelled);
    }
    checkpoint(
        WorkerCheckpoint::AfterPublishIntent,
        &mut deadline,
        cancelled,
    );
    if let Err(stop) = check_work(deadline, cancelled) {
        ledger.aborting = true;
        cleanup_one(
            token,
            &mut ledger,
            crate::operations::executor::SevenZipCreateCleanupTarget::Stage,
            stage,
            controls,
            emit,
        )?;
        ledger.phase = Phase::Discarded;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, &ledger, controls, emit)?;
        return Err(stop);
    }
    let stage_raw = windows::Win32::Foundation::HANDLE(stage.as_raw_handle().cast());
    match super::super::windows::publish_no_replace_handle(stage_raw, destination) {
        Ok(()) => {}
        Err(CopyError::DestinationExists) => {
            ledger.aborting = true;
            cleanup_one(
                token,
                &mut ledger,
                crate::operations::executor::SevenZipCreateCleanupTarget::Stage,
                stage,
                controls,
                emit,
            )?;
            ledger.phase = Phase::Discarded;
            ledger.sequence = ledger
                .sequence
                .checked_add(1)
                .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
            journal(token, &ledger, controls, emit)?;
            return Err(WorkerError::Rejected("output already exists"));
        }
        Err(_) => {
            return Err(WorkerError::Unknown(
                "publish outcome is uncertain; output requires reinspection".into(),
            ));
        }
    }
    checkpoint(
        WorkerCheckpoint::AfterPublishMutation,
        &mut deadline,
        cancelled,
    );
    ledger.stage.state = ObjectState::Published;
    ledger.phase = Phase::Published;
    ledger.archive_identity = ledger.stage.identity.clone();
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    journal(token, &ledger, controls, emit)?;
    Ok(destination.to_path_buf())
}

enum SourcePin {
    File(File),
    Tree(super::super::windows::PinnedSourceTree),
}

fn identity_text(identity: &crate::filesystem_identity::FilesystemIdentity) -> String {
    identity
        .windows_parts()
        .map(|(volume, index)| format!("{volume}:{index:016x}"))
        .unwrap_or_default()
}

fn file_identity(
    file: &File,
) -> Result<crate::operations::journal::JournalSevenZipHandleIdentity, WorkerError> {
    let id = super::super::windows::regular_file_identity(file)
        .map_err(|_| WorkerError::Unknown("owned archive identity could not be read".into()))?;
    Ok(crate::operations::journal::JournalSevenZipHandleIdentity {
        volume: id.volume,
        file_index: (u64::from(id.index_high) << 32) | u64::from(id.index_low),
    })
}

fn hash_file(
    file: &File,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<(u64, [u8; 32]), WorkerError> {
    let mut file = file
        .try_clone()
        .map_err(|_| WorkerError::Rejected("source handle could not be duplicated for capture"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| WorkerError::Rejected("source could not be read"))?;
    let mut hash = Sha256::new();
    let mut length = 0u64;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        check_work(deadline, cancelled)?;
        let read = file
            .read(&mut buffer)
            .map_err(|_| WorkerError::Rejected("source could not be read"))?;
        if read == 0 {
            break;
        }
        length = length
            .checked_add(read as u64)
            .ok_or(WorkerError::Rejected("source length overflow"))?;
        hash.update(&buffer[..read]);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| WorkerError::Rejected("source cursor could not be reset"))?;
    Ok((length, hash.finalize().into()))
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn check_work(deadline: Instant, cancelled: &AtomicBool) -> Result<(), WorkerError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkerError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(WorkerError::Rejected("7z work budget expired"));
    }
    Ok(())
}

fn temporary_sibling(destination: &Path, token: AttemptToken, role: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    destination.with_file_name(format!(
        ".gpui-7z-{}-{}-{}-{sequence}.{role}.tmp",
        std::process::id(),
        token.job_id().value(),
        token.attempt_id().value()
    ))
}

fn build_entries<'a>(
    specs: &[(String, bool, u64, Option<(usize, Option<usize>)>)],
    pins: &'a [SourcePin],
) -> Result<Vec<crate::archive_provider::SevenZipHelperEntry<'a>>, WorkerError> {
    let mut entries = Vec::with_capacity(specs.len());
    for (name, directory, expected_length, source_index) in specs {
        let source = if let Some((pin_index, handle_index)) = source_index {
            match pins.get(*pin_index) {
                Some(SourcePin::File(file)) => Some(file),
                Some(SourcePin::Tree(tree)) => match handle_index
                    .and_then(|index| tree._handles.get(index))
                {
                    Some(Some(super::super::windows::PinnedSourceHandle::File(file))) => Some(file),
                    _ => None,
                },
                _ => None,
            }
        } else {
            None
        };
        entries.push(crate::archive_provider::SevenZipHelperEntry {
            name: name.clone(),
            directory: *directory,
            expected_length: *expected_length,
            source,
        });
    }
    Ok(entries)
}

fn journal(
    token: AttemptToken,
    ledger: &Ledger,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) -> Result<bool, WorkerError> {
    emit(Fact::SevenZipCreateLedgerIntent {
        token,
        ledger: ledger.clone(),
    });
    loop {
        match controls.recv() {
            Ok(WorkerControl::SevenZipCreateLedgerJournaledExact {
                token: ack,
                ledger: saved,
            }) if ack == token && saved == *ledger => return Ok(true),
            Ok(WorkerControl::SevenZipCreateLedgerJournaledCancelledExact {
                token: ack,
                ledger: saved,
            }) if ack == token && saved == *ledger => return Ok(false),
            Ok(WorkerControl::SevenZipCreateLedgerJournalFailedExact {
                token: ack,
                ledger: failed,
            }) if ack == token && failed == *ledger => {
                return Err(WorkerError::Unknown(
                    "7z ledger write failed; operation requires reinspection".into(),
                ));
            }
            Ok(WorkerControl::Cancel) => continue,
            Ok(_) => continue,
            Err(_) => {
                return Err(WorkerError::Unknown(
                    "7z journal channel closed before exact acknowledgment".into(),
                ));
            }
        }
    }
}

fn cleanup_journal(
    token: AttemptToken,
    ledger: &Ledger,
    target: crate::operations::executor::SevenZipCreateCleanupTarget,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) -> Result<(), WorkerError> {
    emit(Fact::SevenZipCreateLedgerIntent {
        token,
        ledger: ledger.clone(),
    });
    loop {
        match controls.recv() {
            Ok(WorkerControl::SevenZipCreateCleanupJournaledExact {
                token: ack,
                ledger: saved,
                target: saved_target,
            }) if ack == token && saved == *ledger && saved_target == target => return Ok(()),
            Ok(WorkerControl::SevenZipCreateLedgerJournaledCancelledExact {
                token: ack,
                ledger: saved,
            }) if ack == token && saved == *ledger => {
                return Err(WorkerError::Unknown(
                    "cleanup journal was durable without exact deletion permission".into(),
                ));
            }
            Ok(WorkerControl::SevenZipCreateLedgerJournalFailedExact {
                token: ack,
                ledger: failed,
            }) if ack == token && failed == *ledger => {
                return Err(WorkerError::Unknown(
                    "cleanup intent journal failed; object ownership requires reinspection".into(),
                ));
            }
            Ok(WorkerControl::Cancel) | Ok(_) => continue,
            Err(_) => {
                return Err(WorkerError::Unknown(
                    "cleanup permission channel closed".into(),
                ));
            }
        }
    }
}

fn cleanup_one(
    token: AttemptToken,
    ledger: &mut Ledger,
    target: crate::operations::executor::SevenZipCreateCleanupTarget,
    file: File,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) -> Result<(), WorkerError> {
    use crate::operations::journal::JournalSevenZipHandleObjectState as ObjectState;
    let object = match target {
        crate::operations::executor::SevenZipCreateCleanupTarget::Candidate => {
            &mut ledger.candidate
        }
        crate::operations::executor::SevenZipCreateCleanupTarget::Stage => &mut ledger.stage,
    };
    if object.state != ObjectState::Owned || Some(file_identity(&file)?) != object.identity {
        return Err(WorkerError::Unknown(
            "owned temporary identity changed before cleanup".into(),
        ));
    }
    object.state = ObjectState::DeleteIntent;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    cleanup_journal(token, ledger, target, controls, emit)?;
    let raw = windows::Win32::Foundation::HANDLE(file.as_raw_handle().cast());
    super::super::windows::set_raw_handle_delete_disposition(raw)
        .map_err(|_| WorkerError::Unknown("exact temporary disposition failed".into()))?;
    drop(file);
    let object = match target {
        crate::operations::executor::SevenZipCreateCleanupTarget::Candidate => {
            &mut ledger.candidate
        }
        crate::operations::executor::SevenZipCreateCleanupTarget::Stage => &mut ledger.stage,
    };
    object.state = ObjectState::Deleted;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    journal(token, ledger, controls, emit).map(|_| ())
}

fn discard_candidate_before_stage(
    token: AttemptToken,
    ledger: &mut Ledger,
    candidate: File,
    failure: WorkerError,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(Fact),
) -> Result<PathBuf, WorkerError> {
    use crate::operations::journal::{
        JournalSevenZipHandleObjectState as ObjectState, JournalSevenZipHandlePhase as Phase,
    };
    ledger.aborting = true;
    cleanup_one(
        token,
        ledger,
        crate::operations::executor::SevenZipCreateCleanupTarget::Candidate,
        candidate,
        controls,
        emit,
    )?;
    if matches!(
        ledger.stage.state,
        ObjectState::Planned | ObjectState::CreateIntent
    ) {
        ledger.stage.state = ObjectState::NeverCreated;
        ledger.sequence = ledger
            .sequence
            .checked_add(1)
            .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
        journal(token, ledger, controls, emit)?;
    }
    ledger.phase = Phase::Discarded;
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerError::Unknown("7z ledger sequence overflow".into()))?;
    journal(token, ledger, controls, emit)?;
    Err(failure)
}

fn copy_exact(
    candidate: &mut File,
    stage: &mut File,
    expected: u64,
    deadline: Instant,
    cancelled: &AtomicBool,
    after_chunk: &mut dyn FnMut(&File, &File, u64),
) -> Result<(), WorkerError> {
    check_work(deadline, cancelled)?;
    candidate
        .seek(SeekFrom::Start(0))
        .map_err(|_| WorkerError::Unknown("candidate seek failed".into()))?;
    stage
        .seek(SeekFrom::Start(0))
        .map_err(|_| WorkerError::Unknown("stage seek failed".into()))?;
    let mut remaining = expected;
    let mut copied = 0u64;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    while remaining != 0 {
        check_work(deadline, cancelled)?;
        let count = buffer.len().min(remaining as usize);
        let read = candidate
            .read(&mut buffer[..count])
            .map_err(|_| WorkerError::Unknown("candidate read failed".into()))?;
        if read == 0 {
            return Err(WorkerError::Unknown(
                "candidate ended before its captured length".into(),
            ));
        }
        stage
            .write_all(&buffer[..read])
            .map_err(|_| WorkerError::Unknown("stage write failed".into()))?;
        remaining -= read as u64;
        copied += read as u64;
        after_chunk(candidate, stage, copied);
    }
    if candidate
        .read(&mut buffer[..1])
        .map_err(|_| WorkerError::Unknown("candidate length check failed".into()))?
        != 0
    {
        return Err(WorkerError::Unknown(
            "candidate changed length during staging".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    #[derive(Clone, Copy)]
    enum FixtureAction {
        CancelAt(WorkerCheckpoint),
        ExpireAt(WorkerCheckpoint),
        CancelAtCandidateDeleteIntent,
        ParentRenameAtPublishIntent,
        FailCandidateIntent,
        FailCandidateOwned,
        CandidateCollision,
        StageCollision,
        PublishCollision,
        CandidateStageCustody,
        CorruptStageCopy,
        ReuseDeletedCandidatePath,
    }

    fn run_worker_fixture(
        app: &Path,
        provider: crate::archive_provider::ArchiveProviderOwner,
        revision: u64,
        binding: crate::archive_provider::ArchiveOperationBinding,
        sources: &[(PathBuf, crate::directory::EntryKind, &'static str)],
        destination: &Path,
        action: FixtureAction,
    ) -> (TerminalReport, crate::operations::journal::JournalStore) {
        let parent_facts =
            crate::operations::model::test_zip_create_guard(&sources[0].0, destination);
        let source_guards = sources
            .iter()
            .map(|(path, kind, name)| {
                let facts = crate::operations::model::test_zip_create_guard_for_kind(
                    path,
                    destination,
                    *kind,
                );
                crate::operations::model::SevenZipCreateSourceGuard {
                    source_identity: facts.source_identity,
                    source_kind: *kind,
                    entry_name: (*name).to_owned(),
                }
            })
            .collect();
        let request = OperationRequest::create_7z(
            sources.iter().map(|(path, _, _)| path.clone()).collect(),
            destination.to_path_buf(),
            crate::operations::model::SevenZipCreateGuard {
                destination_parent_identity: parent_facts.destination_parent_identity,
                sources: source_guards,
                provider_revision: revision,
                provider_binding: binding.clone(),
            },
        )
        .unwrap();
        let mut runtime = crate::operations::runtime::OperationRuntime::new(
            crate::operations::model::OperationService::new(1),
        );
        let token = runtime.submit(request).unwrap();
        let journal_path = destination.with_extension("journal.json");
        let mut store = crate::operations::journal::JournalStore::open(journal_path).unwrap();
        store
            .upsert(runtime.journal_attempt(token).unwrap())
            .unwrap();
        let worker = runtime.start_next().unwrap();
        let crate::operations::executor::ExecutorCommand::Prepare {
            token: command_token,
            request,
            ..
        } = worker.command
        else {
            panic!("Create7z must start as a prepare command");
        };
        assert_eq!(token, command_token);
        let mut terminal = None;
        let mut foreign_paths = Vec::new();
        let mut failed_candidate_path = None;
        let mut failed_candidate_identity = None;
        let mut latest_ledger = None;
        let temp_paths = std::sync::Arc::new(std::sync::Mutex::new(None));
        let emit_temp_paths = temp_paths.clone();
        let mut reused_candidate = None;
        let mut checkpoint = |event: WorkerCheckpoint,
                              deadline: &mut Instant,
                              cancelled: &AtomicBool| {
            match action {
                FixtureAction::CancelAt(target) if event == target => {
                    cancelled.store(true, Ordering::Release);
                }
                FixtureAction::ExpireAt(target) if event == target => {
                    *deadline = Instant::now() - Duration::from_millis(1);
                }
                _ => {}
            }
        };
        let mut emit = |fact| match fact {
            Fact::SevenZipCreateLedgerIntent {
                token: fact_token,
                ledger,
            } => {
                latest_ledger = Some(ledger.clone());
                *emit_temp_paths.lock().unwrap() = Some((
                    PathBuf::from(std::ffi::OsString::from_wide(&ledger.candidate.path.units)),
                    PathBuf::from(std::ffi::OsString::from_wide(&ledger.stage.path.units)),
                ));
                let collision_path = match action {
                    FixtureAction::CandidateCollision
                        if ledger.candidate.state
                            == crate::operations::journal::JournalSevenZipHandleObjectState::CreateIntent =>
                    {
                        Some(PathBuf::from(std::ffi::OsString::from_wide(
                            &ledger.candidate.path.units,
                        )))
                    }
                    FixtureAction::StageCollision
                        if ledger.stage.state
                            == crate::operations::journal::JournalSevenZipHandleObjectState::CreateIntent =>
                    {
                        Some(PathBuf::from(std::ffi::OsString::from_wide(
                            &ledger.stage.path.units,
                        )))
                    }
                    FixtureAction::PublishCollision
                        if ledger.phase
                            == crate::operations::journal::JournalSevenZipHandlePhase::PublishIntent =>
                    {
                        Some(destination.to_path_buf())
                    }
                    _ => None,
                };
                if let Some(path) = collision_path {
                    std::fs::write(&path, b"foreign collision sentinel").unwrap();
                    foreign_paths.push(path);
                }
                if matches!(action, FixtureAction::CancelAtCandidateDeleteIntent)
                    && ledger.candidate.state
                        == crate::operations::journal::JournalSevenZipHandleObjectState::DeleteIntent
                    && ledger.phase
                        == crate::operations::journal::JournalSevenZipHandlePhase::StageReady
                {
                    worker.cancellation.store(true, Ordering::Release);
                }
                if matches!(action, FixtureAction::ParentRenameAtPublishIntent)
                    && ledger.phase
                        == crate::operations::journal::JournalSevenZipHandlePhase::PublishIntent
                {
                    let parent = destination.parent().unwrap();
                    let renamed = parent.with_file_name("worker-parent-renamed");
                    assert!(
                        std::fs::rename(parent, &renamed).is_err(),
                        "retained destination route must prevent parent rename while publication is pending"
                    );
                }
                runtime.commit_fact(Fact::SevenZipCreateLedgerIntent {
                    token: fact_token,
                    ledger: ledger.clone(),
                });
                let failure_requested = matches!(
                    (action, ledger.candidate.state),
                    (
                        FixtureAction::FailCandidateIntent,
                        crate::operations::journal::JournalSevenZipHandleObjectState::CreateIntent
                    ) | (
                        FixtureAction::FailCandidateOwned,
                        crate::operations::journal::JournalSevenZipHandleObjectState::Owned
                    )
                );
                if failure_requested {
                    assert!(runtime.reject_seven_zip_create_ledger(fact_token, &ledger));
                    store
                        .upsert(runtime.journal_attempt(fact_token).unwrap())
                        .unwrap();
                    // 後着のディスク成功は事実として保持するが、失敗ラッチ後なので作業権限にはならない。
                    assert!(runtime.acknowledge_seven_zip_create_ledger(fact_token, &ledger));
                    failed_candidate_path = Some(PathBuf::from(std::ffi::OsString::from_wide(
                        &ledger.candidate.path.units,
                    )));
                    failed_candidate_identity = ledger.candidate.identity.clone();
                    return;
                }
                store
                    .upsert(runtime.journal_attempt(fact_token).unwrap())
                    .unwrap();
                let reopened = crate::operations::journal::JournalStore::open(
                    destination.with_extension("journal.json"),
                )
                .unwrap();
                assert_eq!(reopened.attempt_keys().len(), 1);
                if matches!(
                    ledger.candidate.state,
                    crate::operations::journal::JournalSevenZipHandleObjectState::Owned
                        | crate::operations::journal::JournalSevenZipHandleObjectState::DeleteIntent
                ) {
                    let path =
                        PathBuf::from(std::ffi::OsString::from_wide(&ledger.candidate.path.units));
                    assert!(path.exists(), "reopening must not clean an owned candidate");
                }
                if matches!(
                    ledger.stage.state,
                    crate::operations::journal::JournalSevenZipHandleObjectState::Owned
                        | crate::operations::journal::JournalSevenZipHandleObjectState::DeleteIntent
                ) {
                    let path =
                        PathBuf::from(std::ffi::OsString::from_wide(&ledger.stage.path.units));
                    assert!(path.exists(), "reopening must not clean an owned stage");
                }
                if ledger.phase == crate::operations::journal::JournalSevenZipHandlePhase::Published
                {
                    assert!(
                        destination.is_file(),
                        "reopening a Published snapshot must preserve its output"
                    );
                    assert_eq!(ledger.archive_identity, ledger.stage.identity);
                }
                let mut wrong = ledger.clone();
                wrong.sequence = wrong.sequence.saturating_add(1);
                assert!(!runtime.acknowledge_seven_zip_create_ledger(fact_token, &wrong));
                assert!(runtime.acknowledge_seven_zip_create_ledger(fact_token, &ledger));
                assert!(!runtime.acknowledge_seven_zip_create_ledger(fact_token, &ledger));
                if matches!(action, FixtureAction::ReuseDeletedCandidatePath)
                    && ledger.candidate.state
                        == crate::operations::journal::JournalSevenZipHandleObjectState::Deleted
                    && reused_candidate.is_none()
                {
                    let path =
                        PathBuf::from(std::ffi::OsString::from_wide(&ledger.candidate.path.units));
                    let old_identity = ledger
                        .candidate
                        .identity
                        .clone()
                        .expect("deleted candidate identity");
                    std::fs::write(&path, b"new foreign pathname owner").unwrap();
                    let identity = file_identity(&File::open(&path).unwrap()).unwrap();
                    assert_ne!(Some(identity.clone()), Some(old_identity.clone()));
                    assert!(!runtime.acknowledge_seven_zip_create_ledger(fact_token, &ledger));
                    reused_candidate = Some((path, old_identity, identity));
                }
            }
            Fact::Terminal {
                token: fact_token,
                report,
            } => {
                let final_ledger = latest_ledger
                    .as_ref()
                    .expect("admitted worker must retain its last ledger fact");
                let expected_phase = match action {
                    FixtureAction::FailCandidateIntent | FixtureAction::FailCandidateOwned => {
                        crate::operations::journal::JournalSevenZipHandlePhase::CandidateCreateIntent
                    }
                    FixtureAction::CancelAt(WorkerCheckpoint::AfterPublishMutation)
                    | FixtureAction::ParentRenameAtPublishIntent
                    | FixtureAction::CandidateStageCustody
                    | FixtureAction::ReuseDeletedCandidatePath => {
                        crate::operations::journal::JournalSevenZipHandlePhase::Published
                    }
                    _ => crate::operations::journal::JournalSevenZipHandlePhase::Discarded,
                };
                assert_eq!(final_ledger.phase, expected_phase);
                match action {
                    FixtureAction::FailCandidateIntent => assert_eq!(
                        final_ledger.candidate.state,
                        crate::operations::journal::JournalSevenZipHandleObjectState::CreateIntent
                    ),
                    FixtureAction::FailCandidateOwned => assert_eq!(
                        final_ledger.candidate.state,
                        crate::operations::journal::JournalSevenZipHandleObjectState::Owned
                    ),
                    _ => {}
                }
                runtime.commit_fact(Fact::Terminal {
                    token: fact_token,
                    report: report.clone(),
                });
                store
                    .upsert(runtime.journal_attempt(fact_token).unwrap())
                    .unwrap();
                terminal = Some(report);
            }
            other => {
                runtime.commit_fact(other);
            }
        };
        let mut copy_hook_ran = false;
        let mut copy_hook_bytes_remaining = false;
        let mut copy_chunk = |candidate: &File, stage: &File, copied: u64| {
            if copy_hook_ran || copied < CHUNK_BYTES as u64 {
                return;
            }
            copy_hook_ran = true;
            if matches!(
                action,
                FixtureAction::CandidateStageCustody | FixtureAction::CorruptStageCopy
            ) {
                copy_hook_bytes_remaining = copied < candidate.metadata().unwrap().len();
                assert!(
                    copy_hook_bytes_remaining,
                    "copy probe must run before the final chunk"
                );
            }
            match action {
                FixtureAction::CandidateStageCustody => {
                    let paths = temp_paths
                        .lock()
                        .unwrap()
                        .clone()
                        .expect("ledger temp paths");
                    for path in [&paths.0, &paths.1] {
                        assert!(std::fs::OpenOptions::new().write(true).open(path).is_err());
                        assert!(
                            std::fs::OpenOptions::new()
                                .write(true)
                                .truncate(true)
                                .open(path)
                                .is_err()
                        );
                        assert!(std::fs::remove_file(path).is_err());
                        assert!(std::fs::rename(path, path.with_extension("foreign")).is_err());
                        let replacement = path.with_extension("replacement");
                        std::fs::write(&replacement, b"replacement").unwrap();
                        assert!(std::fs::rename(&replacement, path).is_err());
                        std::fs::remove_file(replacement).unwrap();
                    }
                    assert!(
                        copied < candidate.metadata().unwrap().len(),
                        "custody probe runs before the final copy chunk"
                    );
                    let expected_candidate = file_identity(candidate).unwrap();
                    let expected_stage = file_identity(stage).unwrap();
                    assert_eq!(
                        file_identity(&File::open(&paths.0).unwrap()).unwrap(),
                        expected_candidate
                    );
                    assert_eq!(
                        file_identity(&File::open(&paths.1).unwrap()).unwrap(),
                        expected_stage
                    );
                }
                FixtureAction::CorruptStageCopy => {
                    use std::io::{Seek, SeekFrom, Write};
                    let mut stage = stage;
                    stage.seek(SeekFrom::Start(0)).unwrap();
                    stage.write_all(b"tampered-stage").unwrap();
                    stage.seek(SeekFrom::Start(copied)).unwrap();
                }
                _ => {}
            }
        };
        let image = crate::archive_provider::application_image::lease_explicit_path_for_test(app)
            .expect("fixture must lease the actual application image");
        execute_with_image_for_test_hooks(
            token,
            &request,
            provider,
            image,
            &worker.cancellation,
            &worker.controls,
            &mut emit,
            &mut checkpoint,
            &mut copy_chunk,
        );
        drop(copy_chunk);
        if matches!(
            action,
            FixtureAction::CandidateStageCustody | FixtureAction::CorruptStageCopy
        ) {
            assert!(
                copy_hook_ran,
                "multi-chunk fixture must invoke its copy probe"
            );
            assert!(
                copy_hook_bytes_remaining,
                "probe must observe bytes remaining"
            );
        }
        drop(emit);
        if let Some(path) = failed_candidate_path {
            if let Some(expected) = failed_candidate_identity {
                let held = File::open(&path).expect("owned candidate must remain after failed ACK");
                assert_eq!(file_identity(&held).unwrap(), expected);
                assert!(
                    !PathBuf::from(std::ffi::OsString::from_wide(
                        &latest_ledger.as_ref().unwrap().stage.path.units,
                    ))
                    .exists()
                );
            } else {
                assert!(
                    !path.exists(),
                    "a late exact ACK must not resume CREATE_NEW"
                );
            }
        }
        for path in foreign_paths {
            assert_eq!(std::fs::read(path).unwrap(), b"foreign collision sentinel");
        }
        if let Some((path, old_identity, expected_identity)) = reused_candidate {
            assert_eq!(std::fs::read(&path).unwrap(), b"new foreign pathname owner");
            assert_ne!(old_identity, expected_identity);
            assert_eq!(
                file_identity(&File::open(path).unwrap()).unwrap(),
                expected_identity
            );
        }
        if matches!(action, FixtureAction::CorruptStageCopy) {
            let ledger = latest_ledger
                .as_ref()
                .expect("corruption retains final ledger");
            assert_eq!(
                ledger.phase,
                crate::operations::journal::JournalSevenZipHandlePhase::Discarded
            );
            for object in [&ledger.candidate, &ledger.stage] {
                assert_eq!(
                    object.state,
                    crate::operations::journal::JournalSevenZipHandleObjectState::Deleted
                );
                let path = PathBuf::from(std::ffi::OsString::from_wide(&object.path.units));
                assert!(!path.exists(), "exact owned temp object was cleaned");
            }
        }
        (terminal.expect("worker must emit a terminal report"), store)
    }

    #[test]
    #[ignore = "requires a fresh v6 release executable and installed 7-Zip"]
    fn actual_worker_mixed_roots_are_journaled_and_verified() {
        let app = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("set GPUI_FILER_V6_TEST_EXECUTABLE to the fresh v6 release executable"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("set GPUI_FILER_7ZIP_TEST_EXECUTABLE to the installed 7z.exe"),
        );
        assert!(app.is_absolute() && app.is_file());
        assert!(seven_zip.is_absolute() && seven_zip.is_file());

        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("日本語.txt");
        let mut entropy = 0x9e37_79b9_7f4a_7c15u64;
        let source_bytes = (0..CHUNK_BYTES * 4)
            .map(|_| {
                entropy ^= entropy << 13;
                entropy ^= entropy >> 7;
                entropy ^= entropy << 17;
                entropy as u8
            })
            .collect::<Vec<_>>();
        std::fs::write(&source_path, &source_bytes).unwrap();
        let folder_path = root.path().join("@folder-");
        std::fs::create_dir(&folder_path).unwrap();
        let nested = folder_path.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let nested_path = nested.join("-empty.bin");
        std::fs::write(&nested_path, b"").unwrap();
        let empty_dir = folder_path.join("empty-dir");
        std::fs::create_dir(&empty_dir).unwrap();
        let destination = root.path().join("worker-result.7z");
        let sentinel = root.path().join("sentinel.7z");
        std::fs::write(&sentinel, b"foreign sentinel").unwrap();

        let owner = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = owner.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            root.path().to_path_buf(),
        )
        .expect("installed provider discovery must capture the executable and DLL");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .expect("installed provider must have an operation binding");
        assert!(owner.commit_discovery(revision, Some(executable)));
        let image = crate::archive_provider::application_image::lease_explicit_path_for_test(&app)
            .expect("the actual application image must be leased explicitly");

        let make_source = |path: &Path, kind, name: &str| {
            let facts =
                crate::operations::model::test_zip_create_guard_for_kind(path, &destination, kind);
            crate::operations::model::SevenZipCreateSourceGuard {
                source_identity: facts.source_identity,
                source_kind: kind,
                entry_name: name.to_owned(),
            }
        };
        let parent_facts =
            crate::operations::model::test_zip_create_guard(&source_path, &destination);
        let request = OperationRequest::create_7z(
            vec![source_path.clone(), folder_path.clone()],
            destination.clone(),
            crate::operations::model::SevenZipCreateGuard {
                destination_parent_identity: parent_facts.destination_parent_identity,
                sources: vec![
                    make_source(
                        &source_path,
                        crate::directory::EntryKind::File,
                        "日本語.txt",
                    ),
                    make_source(
                        &folder_path,
                        crate::directory::EntryKind::Folder,
                        "@folder-",
                    ),
                ],
                provider_revision: revision,
                provider_binding: binding.clone(),
            },
        )
        .unwrap();
        let mut runtime = crate::operations::runtime::OperationRuntime::new(
            crate::operations::model::OperationService::new(2),
        );
        let token = runtime.submit(request).unwrap();
        let initial = runtime.journal_attempt(token).unwrap();
        let mut store =
            crate::operations::journal::JournalStore::open(root.path().join("worker-journal.json"))
                .unwrap();
        store.upsert(initial).unwrap();
        let worker = runtime.start_next().unwrap();
        let crate::operations::executor::ExecutorCommand::Prepare {
            token: command_token,
            request,
            ..
        } = worker.command
        else {
            panic!("Create7z must start as a prepare command");
        };
        assert_eq!(command_token, token);
        let mut terminal = None;
        execute_with_image_for_test(
            token,
            &request,
            owner.clone(),
            image,
            &worker.cancellation,
            &worker.controls,
            &mut |fact| match fact {
                Fact::SevenZipCreateLedgerIntent {
                    token: fact_token,
                    ledger,
                } => {
                    runtime.commit_fact(Fact::SevenZipCreateLedgerIntent {
                        token: fact_token,
                        ledger: ledger.clone(),
                    });
                    store
                        .upsert(runtime.journal_attempt(fact_token).unwrap())
                        .unwrap();
                    assert!(runtime.acknowledge_seven_zip_create_ledger(fact_token, &ledger));
                }
                Fact::Terminal {
                    token: fact_token,
                    report,
                } => {
                    runtime.commit_fact(Fact::Terminal {
                        token: fact_token,
                        report: report.clone(),
                    });
                    store
                        .upsert(runtime.journal_attempt(fact_token).unwrap())
                        .unwrap();
                    terminal = Some(report);
                }
                other => {
                    runtime.commit_fact(other);
                }
            },
        );

        let report = terminal.expect("worker must report exactly one terminal result");
        assert_eq!(report.outcome, TerminalOutcome::Succeeded, "{report:?}");
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(
            report.receipts[0].created_archive_path.as_deref(),
            Some(destination.as_path())
        );
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"foreign sentinel");
        let archive = File::open(&destination).unwrap();
        let manifest = [
            super::super::verify::ManifestEntry {
                name: "日本語.txt".into(),
                kind: super::super::verify::EntryKind::File,
                length: source_bytes.len() as u64,
                sha256: Sha256::digest(&source_bytes).into(),
            },
            super::super::verify::ManifestEntry {
                name: "@folder-".into(),
                kind: super::super::verify::EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            super::super::verify::ManifestEntry {
                name: "@folder-/empty-dir".into(),
                kind: super::super::verify::EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            super::super::verify::ManifestEntry {
                name: "@folder-/nested".into(),
                kind: super::super::verify::EntryKind::Directory,
                length: 0,
                sha256: [0; 32],
            },
            super::super::verify::ManifestEntry {
                name: "@folder-/nested/-empty.bin".into(),
                kind: super::super::verify::EntryKind::File,
                length: 0,
                sha256: Sha256::digest([]).into(),
            },
        ];
        let mut archive = archive;
        super::super::verify::verify(
            &mut archive,
            &manifest,
            Instant::now() + Duration::from_secs(30),
            &AtomicBool::new(false),
        )
        .expect("published archive must independently match the captured manifest");
        assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&nested_path).unwrap(), b"");
        let stored = store.recoveries();
        assert!(
            stored.is_empty(),
            "a successful terminal record is not recovery work"
        );

        // CandidateOwned の ACK 後に cancel し、exact DeleteIntent の ACK だけで
        // candidate を閉じて消す abort 経路も、同じ real helper で確認する。
        let cancel_destination = root.path().join("cancelled-result.7z");
        let cancel_parent =
            crate::operations::model::test_zip_create_guard(&source_path, &cancel_destination);
        let cancel_request = OperationRequest::create_7z(
            vec![source_path.clone(), folder_path.clone()],
            cancel_destination.clone(),
            crate::operations::model::SevenZipCreateGuard {
                destination_parent_identity: cancel_parent.destination_parent_identity,
                sources: vec![
                    make_source(
                        &source_path,
                        crate::directory::EntryKind::File,
                        "日本語.txt",
                    ),
                    make_source(
                        &folder_path,
                        crate::directory::EntryKind::Folder,
                        "@folder-",
                    ),
                ],
                provider_revision: revision,
                provider_binding: binding.clone(),
            },
        )
        .unwrap();
        let mut cancelled_runtime = crate::operations::runtime::OperationRuntime::new(
            crate::operations::model::OperationService::new(2),
        );
        let cancelled_token = cancelled_runtime.submit(cancel_request).unwrap();
        let mut cancelled_store = crate::operations::journal::JournalStore::open(
            root.path().join("cancelled-worker-journal.json"),
        )
        .unwrap();
        cancelled_store
            .upsert(cancelled_runtime.journal_attempt(cancelled_token).unwrap())
            .unwrap();
        let cancelled_worker = cancelled_runtime.start_next().unwrap();
        let crate::operations::executor::ExecutorCommand::Prepare {
            token: cancelled_command_token,
            request: cancelled_request,
            ..
        } = cancelled_worker.command
        else {
            panic!("cancelled Create7z must start as a prepare command");
        };
        assert_eq!(cancelled_command_token, cancelled_token);
        let cancel_image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&app)
                .expect("the actual application image must be leased for the second worker");
        let mut cancelled_terminal = None;
        let mut delete_authorized = false;
        execute_with_image_for_test(
            cancelled_token,
            &cancelled_request,
            owner.clone(),
            cancel_image,
            &cancelled_worker.cancellation,
            &cancelled_worker.controls,
            &mut |fact| match fact {
                Fact::SevenZipCreateLedgerIntent {
                    token: fact_token,
                    ledger,
                } => {
                    let candidate_path =
                        PathBuf::from(std::ffi::OsString::from_wide(&ledger.candidate.path.units));
                    let state = ledger.candidate.state;
                    cancelled_runtime.commit_fact(Fact::SevenZipCreateLedgerIntent {
                        token: fact_token,
                        ledger: ledger.clone(),
                    });
                    cancelled_store
                        .upsert(cancelled_runtime.journal_attempt(fact_token).unwrap())
                        .unwrap();
                    assert!(
                        cancelled_runtime.acknowledge_seven_zip_create_ledger(fact_token, &ledger)
                    );
                    if state == crate::operations::journal::JournalSevenZipHandleObjectState::Owned
                        && ledger.phase
                            == crate::operations::journal::JournalSevenZipHandlePhase::CandidateCreateIntent
                    {
                        cancelled_worker
                            .cancellation
                            .store(true, Ordering::Release);
                    }
                    if state
                        == crate::operations::journal::JournalSevenZipHandleObjectState::DeleteIntent
                        && ledger.phase
                            == crate::operations::journal::JournalSevenZipHandlePhase::CandidateCreateIntent
                    {
                        assert!(candidate_path.exists(), "candidate must remain until its exact delete ACK");
                        delete_authorized = true;
                    }
                    if state
                        == crate::operations::journal::JournalSevenZipHandleObjectState::Deleted
                        && ledger.phase
                            == crate::operations::journal::JournalSevenZipHandlePhase::CandidateCreateIntent
                    {
                        assert!(!candidate_path.exists(), "Deleted receipt follows disposition and close");
                    }
                }
                Fact::Terminal {
                    token: fact_token,
                    report,
                } => {
                    cancelled_runtime.commit_fact(Fact::Terminal {
                        token: fact_token,
                        report: report.clone(),
                    });
                    cancelled_store
                        .upsert(cancelled_runtime.journal_attempt(fact_token).unwrap())
                        .unwrap();
                    cancelled_terminal = Some(report);
                }
                other => {
                    cancelled_runtime.commit_fact(other);
                }
            },
        );
        let cancelled_report = cancelled_terminal.expect("cancel path must emit a terminal result");
        assert_eq!(cancelled_report.outcome, TerminalOutcome::Cancelled);
        assert!(
            delete_authorized,
            "the candidate must pass the exact delete-intent barrier"
        );
        assert!(!cancel_destination.exists());
        assert!(cancelled_store.recoveries().is_empty());

        let fixture_sources = [
            (
                source_path.clone(),
                crate::directory::EntryKind::File,
                "日本語.txt",
            ),
            (
                folder_path.clone(),
                crate::directory::EntryKind::Folder,
                "@folder-",
            ),
        ];
        let post_intent_destination = root.path().join("cancel-after-publish-intent.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &post_intent_destination,
            FixtureAction::CancelAt(WorkerCheckpoint::AfterPublishIntent),
        );
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert!(!post_intent_destination.exists());
        assert!(store.recoveries().is_empty());

        let expired_destination = root.path().join("expire-after-publish-intent.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &expired_destination,
            FixtureAction::ExpireAt(WorkerCheckpoint::AfterPublishIntent),
        );
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(!expired_destination.exists());
        assert!(store.recoveries().is_empty());

        let verify_cancel_destination = root.path().join("cancel-during-verify.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &verify_cancel_destination,
            FixtureAction::CancelAt(WorkerCheckpoint::BeforeVerify),
        );
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert!(!verify_cancel_destination.exists());
        assert!(store.recoveries().is_empty());

        let verify_expired_destination = root.path().join("expire-during-verify.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &verify_expired_destination,
            FixtureAction::ExpireAt(WorkerCheckpoint::BeforeVerify),
        );
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert!(!verify_expired_destination.exists());
        assert!(store.recoveries().is_empty());

        let precreate_destination = root.path().join("cancel-before-create.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &precreate_destination,
            FixtureAction::CancelAt(WorkerCheckpoint::BeforeCandidateCreate),
        );
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert!(!precreate_destination.exists());
        assert!(store.recoveries().is_empty());

        let delete_race_destination = root.path().join("cancel-normal-delete-intent.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &delete_race_destination,
            FixtureAction::CancelAtCandidateDeleteIntent,
        );
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert!(!delete_race_destination.exists());
        assert!(store.recoveries().is_empty());

        let prepublish_destination = root.path().join("cancel-before-publish-intent.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &prepublish_destination,
            FixtureAction::CancelAt(WorkerCheckpoint::BeforePublishIntent),
        );
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert!(!prepublish_destination.exists());
        assert!(store.recoveries().is_empty());

        let published_cancel_destination = root.path().join("cancel-after-published.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &published_cancel_destination,
            FixtureAction::CancelAt(WorkerCheckpoint::AfterPublishMutation),
        );
        assert_eq!(report.outcome, TerminalOutcome::Succeeded);
        assert!(published_cancel_destination.is_file());
        assert!(store.recoveries().is_empty());

        let parent_rename_destination = root.path().join("parent-rename-blocked.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &parent_rename_destination,
            FixtureAction::ParentRenameAtPublishIntent,
        );
        assert_eq!(report.outcome, TerminalOutcome::Succeeded);
        assert!(parent_rename_destination.is_file());
        assert!(store.recoveries().is_empty());

        let failed_journal_destination = root.path().join("failed-journal-late-ack.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &failed_journal_destination,
            FixtureAction::FailCandidateIntent,
        );
        assert_ne!(report.outcome, TerminalOutcome::Succeeded);
        assert!(!failed_journal_destination.exists());
        assert!(
            !store.recoveries().is_empty(),
            "failed initial intent remains recovery work after a late disk write"
        );

        let failed_owned_destination = root.path().join("failed-owned-late-ack.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &failed_owned_destination,
            FixtureAction::FailCandidateOwned,
        );
        assert_ne!(report.outcome, TerminalOutcome::Succeeded);
        assert!(!failed_owned_destination.exists());
        assert!(
            !store.recoveries().is_empty(),
            "owned candidate identity remains unresolved after a failed write and late ACK"
        );

        let custody_destination = root.path().join("retained-candidate-stage.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &custody_destination,
            FixtureAction::CandidateStageCustody,
        );
        assert_eq!(report.outcome, TerminalOutcome::Succeeded);
        assert!(custody_destination.is_file());
        assert!(store.recoveries().is_empty());

        let corrupt_destination = root.path().join("corrupt-stage-copy.7z");
        let corrupt_sentinel = root.path().join("corrupt-stage-sentinel.bin");
        std::fs::write(&corrupt_sentinel, b"foreign sentinel").unwrap();
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &corrupt_destination,
            FixtureAction::CorruptStageCopy,
        );
        assert_eq!(report.outcome, TerminalOutcome::Unknown);
        assert!(!corrupt_destination.exists());
        assert_eq!(
            std::fs::read(&corrupt_sentinel).unwrap(),
            b"foreign sentinel"
        );
        assert!(
            store.recoveries().iter().any(|item| match item {
                crate::operations::journal::RecoveryItem::RecoveryRequired(attempt) => {
                    attempt.terminal_outcome
                        == Some(crate::operations::journal::JournalTerminalOutcome::Unknown)
                        && attempt.receipts_recorded
                        && attempt
                            .seven_zip_create_handles
                            .as_ref()
                            .is_some_and(|ledger| {
                                ledger.phase
                            == crate::operations::journal::JournalSevenZipHandlePhase::Discarded
                            })
                }
                crate::operations::journal::RecoveryItem::Unknown { .. } => false,
            }),
            "conservative Unknown remains recoverable while durable ledger records exact Discarded cleanup"
        );

        let reuse_destination = root.path().join("reused-candidate-path.7z");
        let (report, store) = run_worker_fixture(
            &app,
            owner.clone(),
            revision,
            binding.clone(),
            &fixture_sources,
            &reuse_destination,
            FixtureAction::ReuseDeletedCandidatePath,
        );
        assert_eq!(report.outcome, TerminalOutcome::Succeeded);
        assert!(reuse_destination.is_file());
        assert!(store.recoveries().is_empty());

        for (name, action) in [
            ("candidate-collision.7z", FixtureAction::CandidateCollision),
            ("stage-collision.7z", FixtureAction::StageCollision),
            ("publish-collision.7z", FixtureAction::PublishCollision),
        ] {
            let destination = root.path().join(name);
            let (report, store) = run_worker_fixture(
                &app,
                owner.clone(),
                revision,
                binding.clone(),
                &fixture_sources,
                &destination,
                action,
            );
            assert_eq!(report.outcome, TerminalOutcome::Failed);
            assert!(store.recoveries().is_empty());
            if matches!(action, FixtureAction::PublishCollision) {
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    b"foreign collision sentinel"
                );
            } else {
                assert!(!destination.exists());
            }
        }
    }
}
