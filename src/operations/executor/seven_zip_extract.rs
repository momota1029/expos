use super::*;
use crate::archive_provider::{Archive7zExtractionLease, Validated7zExtractPlan};
use crate::operations::journal::{
    JournalPath, JournalSevenZipExtractAttemptId as AttemptId,
    JournalSevenZipExtractBatch as Batch, JournalSevenZipExtractBinding as Binding,
    JournalSevenZipExtractDescriptor as Descriptor, JournalSevenZipExtractEnvelope as Envelope,
    JournalSevenZipExtractFileBinding as FileBinding, JournalSevenZipExtractIdentity as Identity,
    JournalSevenZipExtractIdentity, JournalSevenZipExtractLedger as Ledger,
    JournalSevenZipExtractObjectState as ObjectState,
    JournalSevenZipExtractOutcome as ReceiptOutcome, JournalSevenZipExtractPhase as Phase,
    JournalSevenZipExtractPlan as PlanSummary, JournalSevenZipExtractSource as SourceFact,
    JournalSevenZipExtractSpoolReady as SpoolReady, JournalSevenZipExtractTreeReady as TreeReady,
    SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
};
use std::{
    fs::File,
    os::windows::fs::OpenOptionsExt,
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

const WORK_BUDGET: Duration = Duration::from_secs(60 * 60);

pub(super) fn execute(
    token: AttemptToken,
    request: &super::super::model::OperationRequest,
    attempt: Option<AttemptId>,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    image: Option<crate::archive_provider::application_image::ApplicationImageLease>,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
) {
    execute_with_options(
        token,
        request,
        attempt,
        provider,
        image,
        cancelled,
        controls,
        emit,
        WorkerOptions::default(),
    );
}

#[derive(Default)]
struct WorkerOptions {
    #[cfg(test)]
    fail_after_spool_owned: bool,
    #[cfg(test)]
    fail_after_spool_owned_ordinal: Option<usize>,
    #[cfg(test)]
    fail_spool_working_clone_after_create: bool,
    #[cfg(test)]
    fail_spool_identity_after_create: bool,
    #[cfg(test)]
    fail_tree_identity_after_create: bool,
    #[cfg(test)]
    cancel_during_materialize_after_child: bool,
    #[cfg(test)]
    forget_materializer_child_ids: bool,
    #[cfg(test)]
    expire_publish_deadline_after_intent: bool,
    #[cfg(test)]
    cancel_after_spool_intent_ack: bool,
    #[cfg(test)]
    expire_after_spool_intent_ack: bool,
    #[cfg(test)]
    expire_after_tree_intent_ack: bool,
    #[cfg(test)]
    expire_after_decode: bool,
    #[cfg(test)]
    expire_during_materialize_after_child: bool,
    #[cfg(test)]
    close_uncertain_tree_cleanup: bool,
    #[cfg(test)]
    expire_tree_cleanup_after_identity: bool,
    #[cfg(test)]
    cancel_after_publish_intent_ack: bool,
    #[cfg(test)]
    fail_publish_verification_after_rename: bool,
    #[cfg(test)]
    decode_drain_fixture: Option<DecodeDrainFixture>,
    #[cfg(test)]
    decode_drain_marker: Option<PathBuf>,
    #[cfg(test)]
    decode_drain_completed: Option<std::sync::Arc<AtomicBool>>,
    #[cfg(test)]
    archive_inspection_observed: Option<std::sync::Arc<AtomicBool>>,
    #[cfg(test)]
    stale_provider_rejection_observed: Option<std::sync::Arc<AtomicBool>>,
    #[cfg(test)]
    attempt_context_rejection_observed: Option<std::sync::Arc<AtomicBool>>,
    #[cfg(test)]
    destination_parent_rejection_observed: Option<std::sync::Arc<AtomicBool>>,
    #[cfg(test)]
    tree_cleanup_replay: Option<TreeCleanupReplaySeam>,
}

#[cfg(test)]
#[derive(Clone)]
struct TreeCleanupReplaySeam {
    native_cleanup_finished: std::sync::Arc<std::sync::Barrier>,
    release_successor: std::sync::Arc<std::sync::Barrier>,
    unmatched_control: std::sync::mpsc::Sender<WorkerControl>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum DecodeDrainFixture {
    Cancel,
    Error,
}

fn execute_with_options(
    token: AttemptToken,
    request: &super::super::model::OperationRequest,
    attempt: Option<AttemptId>,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    image: Option<crate::archive_provider::application_image::ApplicationImageLease>,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    options: WorkerOptions,
) {
    let mut held = HeldCustody::default();
    #[cfg(test)]
    {
        held.close_uncertain_tree_cleanup = options.close_uncertain_tree_cleanup;
        held.expire_tree_cleanup_after_identity = options.expire_tree_cleanup_after_identity;
    }
    held.receipts = vec![None; request.sources.len()];
    let await_terminal_release = attempt.is_some();
    let result = match (attempt, provider) {
        (Some(attempt), Some(provider)) => run_batch(
            token, request, attempt, provider, image, cancelled, controls, emit, &mut held,
            &options,
        ),
        _ => Err(WorkerFailure::Rejected(
            "7z extraction capability is unavailable",
        )),
    };
    if held.last_batch.is_none() && result.is_err() {
        if let Err(_failure) = &result {
            #[cfg(test)]
            let failure = _failure;
            #[cfg(test)]
            eprintln!(
                "pre-registration worker failure: {}",
                match failure {
                    WorkerFailure::Cancelled => "cancelled".to_owned(),
                    WorkerFailure::Rejected(reason) => (*reason).to_owned(),
                    WorkerFailure::Unknown(reason) | WorkerFailure::Observed(reason) =>
                        reason.clone(),
                }
            );
        }
        for (ordinal, receipt) in held.receipts.iter_mut().enumerate() {
            *receipt = Some(extract_item_receipt(
                ordinal,
                ItemOutcome::Failed,
                None,
                Some("7z extraction was rejected before batch registration"),
            ));
        }
    }
    let processed = held
        .receipts
        .iter()
        .filter(|receipt| receipt.is_some())
        .count() as u64;
    let (outcome, details, mut receipts, counters) = match result {
        Ok(successes) => (
            if successes == request.sources.len() {
                TerminalOutcome::Succeeded
            } else {
                TerminalOutcome::Partial
            },
            None,
            report_receipts(request, &held, ReceiptOutcome::Completed, None),
            ProgressCounters {
                total_items: Some(request.sources.len() as u64),
                processed_items: successes as u64,
                discovered_items: request.sources.len() as u64,
                processed_bytes: held.verified_bytes,
                ..ProgressCounters::default()
            },
        ),
        Err(WorkerFailure::Cancelled) => (
            if held.uncertain {
                TerminalOutcome::Unknown
            } else {
                TerminalOutcome::Cancelled
            },
            Some("7z extraction was cancelled".to_owned()),
            report_receipts(
                request,
                &held,
                if held.uncertain {
                    ReceiptOutcome::Unknown
                } else {
                    ReceiptOutcome::CancelledBeforeStart
                },
                None,
            ),
            ProgressCounters {
                total_items: Some(request.sources.len() as u64),
                processed_items: processed,
                discovered_items: request.sources.len() as u64,
                processed_bytes: held.verified_bytes,
                ..ProgressCounters::default()
            },
        ),
        Err(WorkerFailure::Rejected(detail)) => (
            if held.uncertain {
                TerminalOutcome::Unknown
            } else {
                TerminalOutcome::Failed
            },
            Some(detail.to_owned()),
            report_receipts(
                request,
                &held,
                if held.uncertain {
                    ReceiptOutcome::Unknown
                } else {
                    ReceiptOutcome::Failed
                },
                None,
            ),
            ProgressCounters {
                total_items: Some(request.sources.len() as u64),
                discovered_items: request.sources.len() as u64,
                processed_items: processed,
                processed_bytes: held.verified_bytes,
                ..ProgressCounters::default()
            },
        ),
        Err(WorkerFailure::Unknown(detail)) => (
            TerminalOutcome::Unknown,
            Some(detail),
            report_receipts(request, &held, ReceiptOutcome::Unknown, None),
            ProgressCounters {
                total_items: Some(request.sources.len() as u64),
                processed_items: processed,
                discovered_items: request.sources.len() as u64,
                processed_bytes: held.verified_bytes,
                ..ProgressCounters::default()
            },
        ),
        Err(WorkerFailure::Observed(detail)) => (
            TerminalOutcome::Unknown,
            Some(detail),
            report_receipts(request, &held, ReceiptOutcome::Unknown, None),
            ProgressCounters {
                total_items: Some(request.sources.len() as u64),
                processed_items: processed,
                discovered_items: request.sources.len() as u64,
                processed_bytes: held.verified_bytes,
                ..ProgressCounters::default()
            },
        ),
    };
    if let Some(detail) = details.as_deref() {
        if let Some(receipt) = receipts
            .iter_mut()
            .find(|receipt| receipt.outcome != ItemOutcome::Completed && receipt.detail.is_none())
        {
            receipt.detail = Some(detail.to_owned());
        }
    }
    let report = TerminalReport {
        outcome,
        final_counters: counters,
        receipts,
        retry: if matches!(outcome, TerminalOutcome::Succeeded) {
            RetryPlan::NotAllowed
        } else {
            RetryPlan::RequiresReinspection
        },
        change_hints: if held.published.is_empty() {
            Vec::new()
        } else {
            request
                .sources
                .first()
                .and_then(|p| p.parent())
                .map(|directory| {
                    vec![FsChangeHint {
                        directory: directory.to_path_buf(),
                    }]
                })
                .unwrap_or_default()
        },
    };
    emit(ExecutorFact::Terminal { token, report });
    // Terminal persistence is a release barrier. Cancellation never turns a missing
    // exact owner reply into permission to drop uncertain custody early.
    if !await_terminal_release {
        return;
    }
    loop {
        match controls.recv() {
            Ok(WorkerControl::Cancel) => {
                held.cancelled = true;
                continue;
            }
            Ok(WorkerControl::SevenZipExtractTerminalJournaledExact { token: ack, batch })
                if ack == token && batch == held.last_batch =>
            {
                break;
            }
            Ok(_) => continue,
            Err(_) => {
                held.abandon_without_mutation = true;
                break;
            }
        }
    }
}

#[derive(Default)]
struct HeldCustody {
    last_batch: Option<Batch>,
    published: Vec<PathBuf>,
    uncertain: bool,
    cancelled: bool,
    abandon_without_mutation: bool,
    _files: Vec<File>,
    // 終端レコードの永続化を待つ間も、出力親の名前空間経路を保持する。
    _parent_route: Vec<super::windows::DirectoryLock>,
    #[cfg(test)]
    close_uncertain_tree_cleanup: bool,
    #[cfg(test)]
    expire_tree_cleanup_after_identity: bool,
    _leases: Vec<Archive7zExtractionLease>,
    _roots: Vec<super::windows::StagingRootLease>,
    _retained_roots: Vec<super::windows::retained_tree::RetainedRoot>,
    _retained_spool_trees: Vec<super::windows::spool_tree::RetainedSpoolTree>,
    _staged: Vec<super::windows::VerifiedStagedTree>,
    _published_trees: Vec<super::windows::VerifiedPublishedTree>,
    _retained_publications: Vec<super::windows::retained_tree::RetainedPublishedTree>,
    _cleanup: Vec<super::windows::extract_cleanup::SpoolDeleteFailure>,
    _staged_cleanup: Vec<super::windows::extract_cleanup::StagedTreeDeleteFailure>,
    receipts: Vec<Option<ItemReceipt>>,
    verified_bytes: u64,
}

#[derive(Debug)]
enum WorkerFailure {
    Cancelled,
    Rejected(&'static str),
    Unknown(String),
    Observed(String),
}

fn report_receipts(
    request: &super::super::model::OperationRequest,
    held: &HeldCustody,
    outcome: ReceiptOutcome,
    publication: Option<(usize, Identity)>,
) -> Vec<ItemReceipt> {
    request
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            if let Some(receipt) = held.receipts.get(index).and_then(Option::as_ref) {
                return receipt.clone();
            }
            let (item_outcome, created) = match (
                outcome,
                publication
                    .as_ref()
                    .filter(|(ordinal, _)| *ordinal == index),
            ) {
                (ReceiptOutcome::Completed, Some(_)) => (
                    ItemOutcome::Completed,
                    source
                        .parent()
                        .and_then(|p| source.file_stem().map(|stem| p.join(stem))),
                ),
                (ReceiptOutcome::Completed, None) => (ItemOutcome::Completed, None),
                (ReceiptOutcome::CancelledBeforeStart, _) => {
                    (ItemOutcome::CancelledBeforeStart, None)
                }
                (ReceiptOutcome::Unknown | ReceiptOutcome::Partial, _) => {
                    (ItemOutcome::Unknown, None)
                }
                _ => (ItemOutcome::Failed, None),
            };
            ItemReceipt {
                item: ItemId(index as u32),
                outcome: item_outcome,
                detail: None,
                created_folder_path: created,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }
        })
        .collect()
}

fn extract_item_receipt(
    ordinal: usize,
    outcome: ItemOutcome,
    created: Option<PathBuf>,
    detail: Option<&str>,
) -> ItemReceipt {
    ItemReceipt {
        item: ItemId(ordinal as u32),
        outcome,
        detail: detail.map(str::to_owned),
        created_folder_path: created,
        created_archive_path: None,
        move_receipt: None,
        replace_receipt: None,
        copy_undo: None,
    }
}

fn run_batch(
    token: AttemptToken,
    request: &super::super::model::OperationRequest,
    attempt: AttemptId,
    provider: crate::archive_provider::ArchiveProviderOwner,
    image_override: Option<crate::archive_provider::application_image::ApplicationImageLease>,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    _options: &WorkerOptions,
) -> Result<usize, WorkerFailure> {
    use crate::operations::model::OperationKind;
    #[cfg(test)]
    let options = _options;
    let guard = request
        .seven_zip_extract_guard
        .as_ref()
        .ok_or(WorkerFailure::Rejected(
            "captured 7z extraction request is invalid",
        ))?;
    if request.kind != OperationKind::Extract7z
        || !request.seven_zip_extract_shape_is_valid()
        || attempt.job != token.job_id().value()
        || attempt.attempt != token.attempt_id().value()
        || attempt.job == 0
        || attempt.attempt == 0
    {
        #[cfg(test)]
        if let Some(observed) = &options.attempt_context_rejection_observed {
            observed.store(true, Ordering::Release);
        }
        return Err(WorkerFailure::Rejected(
            "7z extraction attempt context does not match the request",
        ));
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkerFailure::Cancelled);
    }
    // テスト用fault injectionだけが期限を短縮する。
    #[allow(unused_mut)]
    let mut deadline = Instant::now() + WORK_BUDGET;
    let parent_path = request
        .sources
        .first()
        .and_then(|path| path.parent())
        .ok_or(WorkerFailure::Rejected("7z source parent is missing"))?;
    super::windows::ensure_local_fixed_ntfs(parent_path)
        .map_err(|_| WorkerFailure::Rejected("7z extraction requires a fixed local NTFS parent"))?;
    held._parent_route = match super::windows::lock_virtual_destination(
        parent_path,
        Some(&guard.destination_parent_identity),
    ) {
        Ok(route) => route,
        Err(_) => {
            #[cfg(test)]
            if let Some(observed) = &options.destination_parent_rejection_observed {
                observed.store(true, Ordering::Release);
            }
            return Err(WorkerFailure::Rejected(
                "7z destination parent route changed",
            ));
        }
    };
    let parent_identity = identity_from_fs(guard.destination_parent_identity.clone())?;
    let image = image_override
        .or_else(crate::archive_provider::application_image::lease_for_operation)
        .ok_or(WorkerFailure::Rejected(
            "application image capability is unavailable",
        ))?;
    let app = image.expected_root();
    let app_binding = FileBinding {
        path: JournalPath::from_path(&app.path),
        identity: Identity {
            volume: app.identity.volume_serial,
            file_index: app.identity.file_index,
        },
    };
    let lease =
        match provider.admit_7z_extraction(guard.provider_revision, &guard.provider_binding, image)
        {
            Some(lease) => lease,
            None => {
                #[cfg(test)]
                if let Some(observed) = &options.stale_provider_rejection_observed {
                    observed.store(true, Ordering::Release);
                }
                return Err(WorkerFailure::Rejected(
                    "captured 7z provider binding is stale",
                ));
            }
        };
    held._leases.push(lease.clone());
    let provider_exe = file_binding(&guard.provider_binding.executable);
    let provider_dll = file_binding(&guard.provider_binding.zip_dll);
    let descriptors = build_descriptors(request, &attempt)?;
    let envelope = Envelope {
        attempt: attempt.clone(),
        reservation_format: SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
        provider_revision: guard.provider_revision,
        provider_executable: provider_exe,
        provider_dll,
        application_image: app_binding,
        destination_parent: parent_identity.clone(),
        items: descriptors,
    };
    if envelope.validate().is_err() {
        return Err(WorkerFailure::Rejected(
            "7z extraction batch paths alias or are invalid",
        ));
    }
    let mut batch = Batch {
        envelope,
        sequence: 1,
        items: Vec::new(),
    };
    submit_batch(token, &batch, controls, emit, held, false, None)?;
    let mut completed = 0;
    for ordinal in 0..request.sources.len() {
        check(cancelled, deadline, held)?;
        let source_path = &request.sources[ordinal];
        let guard_source = &guard.sources[ordinal];
        let source = super::windows::open_pinned_file_with_access(source_path, false)
            .map_err(|_| WorkerFailure::Rejected("7z source route is not an ordinary NTFS file"))?;
        let id = super::windows::regular_file_identity(&source)
            .map_err(|_| WorkerFailure::Rejected("7z source identity changed"))?;
        let actual = Identity {
            volume: id.volume,
            file_index: (u64::from(id.index_high) << 32) | u64::from(id.index_low),
        };
        let (expected_volume, expected_index) = guard_source
            .source_identity
            .windows_parts()
            .ok_or(WorkerFailure::Rejected("7z source identity is invalid"))?;
        if actual.volume != expected_volume || actual.file_index != expected_index {
            return Err(WorkerFailure::Rejected("7z source identity changed"));
        }
        let metadata = source
            .metadata()
            .map_err(|_| WorkerFailure::Rejected("7z source signature could not be read"))?;
        let source_signature = SourceFact {
            path: JournalPath::from_path(source_path),
            identity: actual.clone(),
            length: metadata.len(),
            last_write_ticks: last_write_ticks(&metadata)?,
        };
        let output = source_path
            .parent()
            .unwrap()
            .join(source_path.file_stem().unwrap());
        match std::fs::symlink_metadata(&output) {
            Ok(_) => {
                held.receipts[ordinal] = Some(extract_item_receipt(
                    ordinal,
                    ItemOutcome::Failed,
                    None,
                    Some("output already exists"),
                ));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                held.receipts[ordinal] = Some(extract_item_receipt(
                    ordinal,
                    ItemOutcome::Failed,
                    None,
                    Some("output could not be inspected"),
                ));
                continue;
            }
        }
        #[cfg(test)]
        if let Some(observed) = &options.archive_inspection_observed {
            observed.store(true, Ordering::Release);
        }
        let plan =
            match provider.inspect_7z_archive(&lease, &source, parent_path, deadline, cancelled) {
                Ok(plan) => std::sync::Arc::new(plan),
                Err(_) => {
                    held.receipts[ordinal] = Some(extract_item_receipt(
                        ordinal,
                        ItemOutcome::Failed,
                        None,
                        Some("archive inspection failed"),
                    ));
                    continue;
                }
            };
        check(cancelled, deadline, held)?;
        let summary = summarize_plan(&plan)?;
        let descriptor = batch.envelope.items[ordinal].clone();
        let binding = Binding {
            attempt: attempt.clone(),
            item: ordinal as u32,
            source: source_signature,
            provider_revision: batch.envelope.provider_revision,
            provider_executable: batch.envelope.provider_executable.clone(),
            provider_dll: batch.envelope.provider_dll.clone(),
            application_image: batch.envelope.application_image.clone(),
            destination_parent: batch.envelope.destination_parent.clone(),
            output: descriptor.output.clone(),
            spool: descriptor.spool.clone(),
            tree: descriptor.tree.clone(),
            plan: summary.clone(),
        };
        // 項目内 sequence は1から開始し、batch sequenceとは独立して進める。
        let mut ledger = Ledger {
            binding,
            sequence: 1,
            aborting: false,
            phase: Phase::SpoolCreateIntent,
            spool_state: ObjectState::CreateIntent,
            spool_identity: None,
            tree_state: ObjectState::Planned,
            tree_identity: None,
            spool_ready: None,
            tree_ready: None,
            publication_identity: None,
        };
        held._files.push(
            source.try_clone().map_err(|_| {
                WorkerFailure::Unknown("7z source pin could not be retained".into())
            })?,
        );
        append_initial_ledger(&mut batch, &ledger)?;
        // From this durable intent onward an object or an unresolved native result
        // may exist, so every early exit must remain an inspection-only Unknown.
        held.uncertain = true;
        match submit_batch(token, &batch, controls, emit, held, false, None) {
            Ok(()) if held.cancelled => {
                return discard_uncreated_spool_after_intent(
                    token,
                    ordinal,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    true,
                );
            }
            Ok(()) => {}
            Err(WorkerFailure::Observed(_)) => {
                return discard_uncreated_spool_after_intent(
                    token,
                    ordinal,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    true,
                );
            }
            Err(error) => return Err(error),
        }
        #[cfg(test)]
        if options.cancel_after_spool_intent_ack && ledger.phase == Phase::SpoolCreateIntent {
            held.cancelled = true;
        }
        #[cfg(test)]
        if options.expire_after_spool_intent_ack && ledger.phase == Phase::SpoolCreateIntent {
            // 実際の固定deadline判定を通すため、intent ACK後に期限を過去へ進める。
            deadline = Instant::now() - Duration::from_millis(1);
        }
        if let Err(stop) = check(cancelled, deadline, held) {
            if matches!(stop, WorkerFailure::Cancelled) {
                return discard_uncreated_spool_after_intent(
                    token,
                    ordinal,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    true,
                );
            }
            if matches!(
                &stop,
                WorkerFailure::Rejected("7z extraction work deadline expired")
            ) {
                return discard_uncreated_spool_after_intent(
                    token,
                    ordinal,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    false,
                );
            }
            return Err(stop);
        }
        verify_source_signature(&source, &ledger.binding.source)?;
        let spool_path = path_from_journal(&descriptor.spool)?;
        let spool_owner = match create_spool(&spool_path) {
            Ok(file) => file,
            Err(_) => {
                held.uncertain = true;
                return Err(WorkerFailure::Unknown(
                    "spool CREATE_NEW failed after its durable intent".into(),
                ));
            }
        };
        // CREATE_NEW の返却直後に original handle を custody へ移し、
        // 以後の複製・identity query が失敗しても terminal release まで保持する。
        held._files.push(spool_owner);
        #[cfg(test)]
        if options.fail_spool_working_clone_after_create {
            return Err(WorkerFailure::Unknown(
                "test-injected working handle clone failure after CREATE_NEW".into(),
            ));
        }
        let mut spool = held
            ._files
            .last()
            .expect("newly created spool is immediately in worker custody")
            .try_clone()
            .map_err(|_| WorkerFailure::Unknown("spool working clone failed".into()))?;
        #[cfg(test)]
        if options.fail_spool_identity_after_create {
            return Err(WorkerFailure::Unknown(
                "test-injected spool identity observation failure after CREATE_NEW".into(),
            ));
        }
        let spool_identity = file_identity(&spool)?;
        advance(
            &mut batch,
            &mut ledger,
            Phase::SpoolOwned,
            ObjectState::Owned,
            Some(spool_identity.clone()),
            ObjectState::Planned,
            None,
            None,
            None,
        )?;
        match submit_batch(token, &batch, controls, emit, held, false, None) {
            Ok(()) => {}
            Err(WorkerFailure::Observed(_)) => {
                // Observed は SpoolOwned の記録だけを確認する。後続の作成権限にはせず、
                // exact cleanup intent を別 ACK で得てから保持中の同一 handle を片付ける。
                advance_abort_latch(&mut batch, &mut ledger)?;
                match submit_batch(token, &batch, controls, emit, held, false, None) {
                    Ok(()) | Err(WorkerFailure::Observed(_)) => {}
                    Err(error) => return Err(error),
                }
                advance(
                    &mut batch,
                    &mut ledger,
                    Phase::SpoolOwned,
                    ObjectState::DeleteIntent,
                    Some(spool_identity.clone()),
                    ObjectState::Planned,
                    None,
                    None,
                    None,
                )?;
                submit_batch(
                    token,
                    &batch,
                    controls,
                    emit,
                    held,
                    true,
                    Some((
                        ItemId(ordinal as u32),
                        super::super::runtime::SevenZipExtractCleanupTarget::Spool,
                    )),
                )?;
                held._files.pop();
                let parent_identity = identity_from_fs(
                    request
                        .seven_zip_extract_guard
                        .as_ref()
                        .ok_or(WorkerFailure::Rejected("7z guard is missing"))?
                        .destination_parent_identity
                        .clone(),
                )?;
                let cleanup_deadline = Instant::now() + Duration::from_secs(30);
                if let Err(failure) = super::windows::extract_cleanup::delete_spool(
                    spool,
                    &spool_identity,
                    parent_path,
                    &parent_identity,
                    cleanup_deadline,
                ) {
                    held._cleanup.push(failure);
                    held.uncertain = true;
                    return Err(WorkerFailure::Unknown(
                        "cancelled spool cleanup did not confirm exact identity absence".into(),
                    ));
                }
                advance(
                    &mut batch,
                    &mut ledger,
                    Phase::SpoolOwned,
                    ObjectState::Deleted,
                    Some(spool_identity.clone()),
                    ObjectState::Planned,
                    None,
                    None,
                    None,
                )?;
                match submit_batch(token, &batch, controls, emit, held, false, None) {
                    Ok(()) | Err(WorkerFailure::Observed(_)) => {}
                    Err(error) => return Err(error),
                }
                advance(
                    &mut batch,
                    &mut ledger,
                    Phase::SpoolOwned,
                    ObjectState::Deleted,
                    Some(spool_identity.clone()),
                    ObjectState::NeverCreated,
                    None,
                    None,
                    None,
                )?;
                match submit_batch(token, &batch, controls, emit, held, false, None) {
                    Ok(()) | Err(WorkerFailure::Observed(_)) => {}
                    Err(error) => return Err(error),
                }
                advance(
                    &mut batch,
                    &mut ledger,
                    Phase::Discarded,
                    ObjectState::Deleted,
                    Some(spool_identity),
                    ObjectState::NeverCreated,
                    None,
                    None,
                    None,
                )?;
                match submit_batch(token, &batch, controls, emit, held, false, None) {
                    Ok(()) | Err(WorkerFailure::Observed(_)) => {}
                    Err(error) => return Err(error),
                }
                held.receipts[ordinal] = Some(extract_item_receipt(
                    ordinal,
                    ItemOutcome::CancelledBeforeStart,
                    None,
                    Some("7z extraction was cancelled after spool creation"),
                ));
                held.uncertain = false;
                return Err(WorkerFailure::Cancelled);
            }
            Err(error) => return Err(error),
        }
        #[cfg(test)]
        if options.fail_after_spool_owned || options.fail_after_spool_owned_ordinal == Some(ordinal)
        {
            return Err(WorkerFailure::Unknown(
                "test-injected spool-only uncertainty".into(),
            ));
        }
        check(cancelled, deadline, held)?;
        #[cfg(test)]
        let decode_result = if let Some(fixture) = options.decode_drain_fixture {
            let marker = options
                .decode_drain_marker
                .as_deref()
                .expect("drain fixture has a child marker path");
            let drained = options
                .decode_drain_completed
                .as_deref()
                .expect("drain fixture has an observation flag");
            let mode = match fixture {
                DecodeDrainFixture::Cancel => "descendant_with_dll",
                DecodeDrainFixture::Error => "descendant_error_with_dll",
            };
            Err(
                crate::archive_provider::run_worker_decode_drain_fixture_for_test(
                    mode, marker, deadline, cancelled, drained,
                ),
            )
        } else {
            provider.decode_7z_archive(
                lease.clone(),
                &source,
                &mut spool,
                plan.clone(),
                parent_path,
                deadline,
                cancelled,
            )
        };
        #[cfg(not(test))]
        let decode_result = provider.decode_7z_archive(
            lease.clone(),
            &source,
            &mut spool,
            plan.clone(),
            parent_path,
            deadline,
            cancelled,
        );
        if let Err(decode_failure) = decode_result {
            // 子プロセスとJob Object内の子孫はdecode呼出しから戻る時点でdrain済みなので、
            // 未公開のCREATE_NEW spoolを正確なcleanup intentの下で閉じられる。
            let was_cancelled = matches!(
                decode_failure,
                crate::archive_provider::SevenZipDecodeFailure::Cancelled
            );
            return match abort_owned_spool_without_tree(
                token,
                ordinal,
                spool,
                &parent_identity,
                parent_path,
                &mut batch,
                &mut ledger,
                controls,
                emit,
                held,
                was_cancelled,
            ) {
                Err(failure) => Err(failure),
                Ok(()) if was_cancelled => Err(WorkerFailure::Cancelled),
                Ok(()) => Err(WorkerFailure::Rejected("7z decoder failed")),
            };
        }
        #[cfg(test)]
        if options.expire_after_decode {
            // 実 provider の drain 完了後に固定deadlineを期限切れへ進める。
            deadline = Instant::now() - Duration::from_millis(1);
        }
        if let Err(stop) = check(cancelled, deadline, held) {
            let cancelled_stop = matches!(stop, WorkerFailure::Cancelled);
            let expired_stop = matches!(
                &stop,
                WorkerFailure::Rejected("7z extraction work deadline expired")
            );
            if cancelled_stop || expired_stop {
                abort_owned_spool_without_tree(
                    token,
                    ordinal,
                    spool,
                    &parent_identity,
                    parent_path,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    cancelled_stop,
                )?;
            }
            return Err(stop);
        }
        verify_source_signature(&source, &ledger.binding.source)?;
        let spool_ready = SpoolReady {
            plan_format: 1,
            plan_sha256: summary.sha256,
            length: summary.total_expanded_bytes,
        };
        advance(
            &mut batch,
            &mut ledger,
            Phase::SpoolReady,
            ObjectState::Owned,
            Some(spool_identity.clone()),
            ObjectState::Planned,
            None,
            Some(spool_ready),
            None,
        )?;
        match submit_batch(token, &batch, controls, emit, held, false, None) {
            Ok(()) => {}
            Err(WorkerFailure::Observed(_)) => {
                abort_owned_spool_without_tree(
                    token,
                    ordinal,
                    spool,
                    &parent_identity,
                    parent_path,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    true,
                )?;
                return Err(WorkerFailure::Cancelled);
            }
            Err(error) => return Err(error),
        }
        if let Err(stop) = check(cancelled, deadline, held) {
            let cancelled_stop = matches!(stop, WorkerFailure::Cancelled);
            let expired_stop = matches!(
                &stop,
                WorkerFailure::Rejected("7z extraction work deadline expired")
            );
            if cancelled_stop || expired_stop {
                abort_owned_spool_without_tree(
                    token,
                    ordinal,
                    spool,
                    &parent_identity,
                    parent_path,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    cancelled_stop,
                )?;
            }
            return Err(stop);
        }
        advance(
            &mut batch,
            &mut ledger,
            Phase::TreeCreateIntent,
            ObjectState::Owned,
            Some(spool_identity.clone()),
            ObjectState::CreateIntent,
            None,
            None,
            None,
        )?;
        match submit_batch(token, &batch, controls, emit, held, false, None) {
            Ok(()) => {}
            Err(WorkerFailure::Observed(_)) => {
                abort_owned_spool_without_tree(
                    token,
                    ordinal,
                    spool,
                    &parent_identity,
                    parent_path,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    true,
                )?;
                return Err(WorkerFailure::Cancelled);
            }
            Err(error) => return Err(error),
        }
        #[cfg(test)]
        if options.expire_after_tree_intent_ack && ledger.phase == Phase::TreeCreateIntent {
            // native root CREATE_NEWの直前で実期限切れを評価する。
            deadline = Instant::now() - Duration::from_millis(1);
        }
        if let Err(stop) = check(cancelled, deadline, held) {
            if matches!(stop, WorkerFailure::Cancelled)
                || matches!(
                    &stop,
                    WorkerFailure::Rejected("7z extraction work deadline expired")
                )
            {
                abort_owned_spool_without_tree(
                    token,
                    ordinal,
                    spool,
                    &parent_identity,
                    parent_path,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    matches!(stop, WorkerFailure::Cancelled),
                )?;
            }
            return Err(stop);
        }
        let tree_path = path_from_journal(&descriptor.tree)?;
        #[cfg(test)]
        let root_result = if options.fail_tree_identity_after_create {
            super::windows::retained_tree::create_root_with_test_identity_failure(&tree_path)
        } else {
            super::windows::retained_tree::create_root(&tree_path)
        };
        #[cfg(not(test))]
        let root_result = super::windows::retained_tree::create_root(&tree_path);
        let root = match root_result {
            super::windows::retained_tree::CreateRoot::Ready(root) => root,
            super::windows::retained_tree::CreateRoot::Created { root, .. } => {
                held._retained_roots.push(root);
                held.uncertain = true;
                return Err(WorkerFailure::Unknown(
                    "tree root was created but its identity is uncertain".into(),
                ));
            }
            super::windows::retained_tree::CreateRoot::BeforeCreate(_) => {
                held.uncertain = true;
                return Err(WorkerFailure::Unknown(
                    "tree root CREATE_NEW failed after its durable intent".into(),
                ));
            }
        };
        let tree_identity = identity_from_text(&root.root_identity)?;
        held._roots.push(root);
        advance(
            &mut batch,
            &mut ledger,
            Phase::TreeOwned,
            ObjectState::Owned,
            Some(spool_identity.clone()),
            ObjectState::Owned,
            Some(tree_identity.clone()),
            None,
            None,
        )?;
        submit_batch(token, &batch, controls, emit, held, false, None)?;
        check(cancelled, deadline, held)?;
        let root = held
            ._roots
            .pop()
            .expect("TreeOwned root remains in worker custody");
        #[cfg(test)]
        let materialize_result = if options.expire_during_materialize_after_child {
            super::windows::spool_tree::materialize_expire_after_first_write_for_test(
                &mut spool,
                spool_identity.clone(),
                plan.clone(),
                root,
                cancelled,
                deadline,
            )
        } else if options.cancel_during_materialize_after_child {
            super::windows::spool_tree::materialize_cancel_after_first_write_for_test(
                &mut spool,
                spool_identity.clone(),
                plan.clone(),
                root,
                cancelled,
                deadline,
            )
        } else {
            super::windows::spool_tree::materialize(
                &mut spool,
                spool_identity.clone(),
                plan.clone(),
                root,
                cancelled,
                deadline,
            )
        };
        #[cfg(not(test))]
        let materialize_result = super::windows::spool_tree::materialize(
            &mut spool,
            spool_identity.clone(),
            plan.clone(),
            root,
            cancelled,
            deadline,
        );
        let (staged, bytes) = match materialize_result {
            Ok(success) => success,
            Err(failure) => {
                #[cfg(test)]
                let mut failure = failure;
                #[cfg(test)]
                if options.forget_materializer_child_ids {
                    for child in &mut failure.retained.children {
                        child.observed_identity = None;
                    }
                }
                return discard_retained_tree_after_materialize_failure(
                    token,
                    ordinal,
                    spool,
                    *failure.retained,
                    &parent_identity,
                    parent_path,
                    &mut batch,
                    &mut ledger,
                    controls,
                    emit,
                    held,
                    cancelled,
                    #[cfg(test)]
                    options.tree_cleanup_replay.clone(),
                );
            }
        };
        let tree_ready = TreeReady {
            plan_format: 1,
            plan_sha256: summary.sha256,
            materialized_node_count: summary.materialized_node_count,
            file_count: summary.file_count,
            total_verified_bytes: bytes,
        };
        advance(
            &mut batch,
            &mut ledger,
            Phase::TreeReady,
            ObjectState::Owned,
            Some(spool_identity.clone()),
            ObjectState::Owned,
            Some(tree_identity.clone()),
            None,
            Some(tree_ready.clone()),
        )?;
        held._staged.push(staged);
        submit_batch(token, &batch, controls, emit, held, false, None)?;
        advance(
            &mut batch,
            &mut ledger,
            Phase::TreeReady,
            ObjectState::DeleteIntent,
            Some(spool_identity.clone()),
            ObjectState::Owned,
            Some(tree_identity.clone()),
            None,
            Some(tree_ready),
        )?;
        submit_batch(
            token,
            &batch,
            controls,
            emit,
            held,
            true,
            Some((
                ItemId(ordinal as u32),
                super::super::runtime::SevenZipExtractCleanupTarget::Spool,
            )),
        )?;
        // The spool clone retained for failure custody must close before a fresh
        // parent scan can prove the deleted file ID absent.
        held._files.pop();
        let spool_identity_full = spool_identity.clone();
        let parent_identity_full = identity_from_fs(guard.destination_parent_identity.clone())?;
        let cleanup_deadline = Instant::now() + Duration::from_secs(30);
        if let Err(failure) = super::windows::extract_cleanup::delete_spool(
            spool,
            &spool_identity_full,
            parent_path,
            &parent_identity_full,
            cleanup_deadline,
        ) {
            held._cleanup.push(failure);
            held.uncertain = true;
            return Err(WorkerFailure::Unknown(
                "spool cleanup did not confirm old identity absence".into(),
            ));
        }
        advance(
            &mut batch,
            &mut ledger,
            Phase::SpoolCleaned,
            ObjectState::Deleted,
            Some(spool_identity.clone()),
            ObjectState::Owned,
            Some(tree_identity.clone()),
            None,
            None,
        )?;
        submit_batch(token, &batch, controls, emit, held, false, None)?;
        if let Err(stop) = check(cancelled, deadline, held) {
            discard_staged_after_spool_cleaned(
                token,
                ordinal,
                &parent_identity_full,
                parent_path,
                &mut batch,
                &mut ledger,
                controls,
                emit,
                held,
                cleanup_deadline,
            )?;
            held.uncertain = false;
            held.receipts[ordinal] = Some(extract_item_receipt(
                ordinal,
                if matches!(stop, WorkerFailure::Cancelled) {
                    ItemOutcome::CancelledBeforeStart
                } else {
                    ItemOutcome::Failed
                },
                None,
                Some("7z extraction stopped before publication"),
            ));
            return Err(stop);
        }
        advance(
            &mut batch,
            &mut ledger,
            Phase::PublishIntent,
            ObjectState::Deleted,
            Some(spool_identity.clone()),
            ObjectState::Owned,
            Some(tree_identity.clone()),
            None,
            None,
        )?;
        #[cfg(test)]
        if options.expire_publish_deadline_after_intent {
            deadline = Instant::now() + Duration::from_millis(100);
        }
        submit_batch(token, &batch, controls, emit, held, false, None)?;
        #[cfg(test)]
        if options.cancel_after_publish_intent_ack {
            // PublishIntentは取消後も掃除へ戻せないため、ACK直後に実cancelを記録する。
            cancelled.store(true, Ordering::Release);
        }
        check(cancelled, deadline, held)?;
        // PublishIntent is irreversible. Every adapter failure below keeps its returned custody.
        let destination = path_from_journal(&descriptor.output)?;
        let staged = held
            ._staged
            .pop()
            .expect("unpublished stage remains in worker custody");
        #[cfg(test)]
        let publish_result = if options.fail_publish_verification_after_rename {
            super::windows::retained_tree::publish_fail_root_reopen_after_rename_for_test(
                staged,
                &destination,
                cancelled,
            )
        } else {
            super::windows::retained_tree::publish(staged, &destination, cancelled)
        };
        #[cfg(not(test))]
        let publish_result =
            super::windows::retained_tree::publish(staged, &destination, cancelled);
        let published = match publish_result {
            Ok(published) => published,
            Err(super::windows::retained_tree::PublishRetainedFailure::BeforeRename {
                staged,
                ..
            }) => {
                held._staged.push(staged);
                held.uncertain = true;
                return Err(WorkerFailure::Unknown(
                    "publication barrier was crossed before a failed rename".into(),
                ));
            }
            Err(super::windows::retained_tree::PublishRetainedFailure::RenameAttempted {
                retained,
                ..
            }) => {
                held._retained_publications.push(retained);
                held.uncertain = true;
                return Err(WorkerFailure::Unknown(
                    "publication outcome could not be verified".into(),
                ));
            }
        };
        // 公開に成功した handle 群は Published 記録が確定するまで worker custody に移す。
        // identity 解析や journal submit が失敗しても、終端保存待ち中に名前を動かせない。
        held._published_trees.push(published);
        let publication_identity = identity_from_text(
            &held
                ._published_trees
                .last()
                .expect("published tree entered custody")
                .root_identity,
        )?;
        ledger.publication_identity = Some(publication_identity.clone());
        advance(
            &mut batch,
            &mut ledger,
            Phase::Published,
            ObjectState::Deleted,
            Some(spool_identity),
            ObjectState::Published,
            Some(publication_identity.clone()),
            None,
            None,
        )?;
        // 公開済みの事実は記録応答前に保持し、Observed ACKでも既知の成功を失わない。
        held.published.push(destination.clone());
        held.receipts[ordinal] = Some(extract_item_receipt(
            ordinal,
            ItemOutcome::Completed,
            Some(destination.clone()),
            None,
        ));
        match submit_batch(token, &batch, controls, emit, held, false, None) {
            Ok(()) | Err(WorkerFailure::Observed(_)) => {
                // Exact Published snapshot was saved or observed; this clean prefix needs no pin.
                held._published_trees.pop();
            }
            Err(error) => return Err(error),
        }
        held.verified_bytes = held.verified_bytes.saturating_add(bytes);
        completed += 1;
        held.uncertain = false;
        emit(ExecutorFact::Progress {
            token,
            update: ProgressUpdate {
                sequence: ordinal as u64 + 1,
                counters: ProgressCounters {
                    total_items: Some(request.sources.len() as u64),
                    discovered_items: request.sources.len() as u64,
                    processed_items: completed as u64,
                    processed_bytes: held.verified_bytes,
                    ..ProgressCounters::default()
                },
                current_item: Some(ItemId(ordinal as u32)),
            },
        });
        // The durable Published observation releases this item's source/spool pins.
        held._files.clear();
    }
    Ok(completed)
}

fn discard_staged_after_spool_cleaned(
    token: AttemptToken,
    ordinal: usize,
    parent_identity: &Identity,
    parent_path: &std::path::Path,
    batch: &mut Batch,
    ledger: &mut Ledger,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    cleanup_deadline: Instant,
) -> Result<(), WorkerFailure> {
    let spool_identity = ledger.spool_identity.clone();
    let tree_identity = ledger.tree_identity.clone();
    advance_abort_latch(batch, ledger)?;
    match submit_batch(token, batch, controls, emit, held, false, None) {
        Ok(()) | Err(WorkerFailure::Observed(_)) => {}
        Err(error) => return Err(error),
    }
    advance(
        batch,
        ledger,
        Phase::SpoolCleaned,
        ObjectState::Deleted,
        spool_identity.clone(),
        ObjectState::DeleteIntent,
        tree_identity.clone(),
        None,
        None,
    )?;
    submit_batch(
        token,
        batch,
        controls,
        emit,
        held,
        true,
        Some((
            ItemId(ordinal as u32),
            super::super::runtime::SevenZipExtractCleanupTarget::Tree,
        )),
    )?;
    let staged = held._staged.pop().ok_or_else(|| {
        WorkerFailure::Unknown("staged tree custody disappeared before abort cleanup".into())
    })?;
    if let Err(failure) = delete_staged_tree_with_acknowledged_intent(
        staged,
        parent_identity,
        parent_path,
        cleanup_deadline,
        ledger,
        controls,
        held,
    ) {
        held._staged_cleanup.push(failure);
        held.uncertain = true;
        return Err(WorkerFailure::Unknown(
            "staging tree cleanup did not confirm exact identity absence".into(),
        ));
    }
    advance(
        batch,
        ledger,
        Phase::SpoolCleaned,
        ObjectState::Deleted,
        spool_identity.clone(),
        ObjectState::Deleted,
        tree_identity.clone(),
        None,
        None,
    )?;
    match submit_batch(token, batch, controls, emit, held, false, None) {
        Ok(()) | Err(WorkerFailure::Observed(_)) => {}
        Err(error) => return Err(error),
    }
    advance(
        batch,
        ledger,
        Phase::Discarded,
        ObjectState::Deleted,
        spool_identity,
        ObjectState::Deleted,
        tree_identity,
        None,
        None,
    )?;
    submit_batch(token, batch, controls, emit, held, false, None)
}

fn discard_uncreated_spool_after_intent(
    token: AttemptToken,
    ordinal: usize,
    batch: &mut Batch,
    ledger: &mut Ledger,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    cancelled: bool,
) -> Result<usize, WorkerFailure> {
    // CREATE_NEW前なら、取消と期限切れを区別してNeverCreatedを順に保存する。
    advance_abort_latch(batch, ledger)?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        Phase::SpoolCreateIntent,
        ObjectState::NeverCreated,
        None,
        ObjectState::Planned,
        None,
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        Phase::SpoolCreateIntent,
        ObjectState::NeverCreated,
        None,
        ObjectState::NeverCreated,
        None,
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        Phase::Discarded,
        ObjectState::NeverCreated,
        None,
        ObjectState::NeverCreated,
        None,
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    held.receipts[ordinal] = Some(extract_item_receipt(
        ordinal,
        if cancelled {
            ItemOutcome::CancelledBeforeStart
        } else {
            ItemOutcome::Failed
        },
        None,
        Some(if cancelled {
            "7z extraction was cancelled before spool creation"
        } else {
            "7z extraction deadline expired before spool creation"
        }),
    ));
    held.uncertain = false;
    Err(if cancelled {
        WorkerFailure::Cancelled
    } else {
        WorkerFailure::Rejected("7z extraction work deadline expired")
    })
}

fn delete_staged_tree_with_acknowledged_intent(
    staged: super::windows::VerifiedStagedTree,
    parent_identity: &Identity,
    parent_path: &std::path::Path,
    cleanup_deadline: Instant,
    ledger: &Ledger,
    controls: &Receiver<WorkerControl>,
    held: &mut HeldCustody,
) -> Result<(), super::windows::extract_cleanup::StagedTreeDeleteFailure> {
    use super::windows::extract_cleanup::{delete_staged_tree, resume_staged_tree_delete};

    let expected_parent = JournalSevenZipExtractIdentity {
        volume: parent_identity.volume,
        file_index: parent_identity.file_index,
    };
    let mut failure =
        match delete_staged_tree(staged, &expected_parent, parent_path, cleanup_deadline) {
            Ok(()) => return Ok(()),
            Err(failure) => failure,
        };

    loop {
        if failure.reason != "staged child identity must be recorded before cleanup"
            || ledger.phase != Phase::SpoolCleaned
            || ledger.tree_state != ObjectState::DeleteIntent
            || !ledger.aborting
            || Instant::now() >= cleanup_deadline
        {
            return Err(failure);
        }
        if !cleanup_resume_controls_allow(controls, held, cleanup_deadline) {
            return Err(failure);
        }
        failure = match resume_staged_tree_delete(
            failure,
            &expected_parent,
            parent_path,
            cleanup_deadline,
        ) {
            Ok(()) => return Ok(()),
            Err(failure) => failure,
        };
    }
}

fn discard_retained_tree_after_materialize_failure(
    token: AttemptToken,
    ordinal: usize,
    spool: File,
    retained: super::windows::spool_tree::RetainedSpoolTree,
    parent_identity: &Identity,
    parent_path: &std::path::Path,
    batch: &mut Batch,
    ledger: &mut Ledger,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    cancelled: &AtomicBool,
    #[cfg(test)] tree_cleanup_replay: Option<TreeCleanupReplaySeam>,
) -> Result<usize, WorkerFailure> {
    use super::windows::extract_cleanup::{delete_retained_spool_tree, resume_staged_tree_delete};

    let cleanup_deadline = Instant::now() + Duration::from_secs(30);
    let spool_identity = ledger.spool_identity.clone().ok_or_else(|| {
        WorkerFailure::Unknown("spool identity missing during tree cleanup".into())
    })?;
    let tree_identity = ledger.tree_identity.clone().ok_or_else(|| {
        WorkerFailure::Unknown("tree identity missing during materialization cleanup".into())
    })?;
    // 観測済みrootと部分childは、delete intent の永続応答より先に custody へ戻す。
    held._retained_spool_trees.push(retained);
    advance_abort_latch(batch, ledger)?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        ledger.phase,
        ObjectState::Owned,
        Some(spool_identity.clone()),
        ObjectState::DeleteIntent,
        Some(tree_identity.clone()),
        None,
        None,
    )?;
    submit_batch(
        token,
        batch,
        controls,
        emit,
        held,
        true,
        Some((
            ItemId(ordinal as u32),
            super::super::runtime::SevenZipExtractCleanupTarget::Tree,
        )),
    )?;

    let retained = held._retained_spool_trees.pop().ok_or_else(|| {
        WorkerFailure::Unknown("partial tree custody disappeared before cleanup".into())
    })?;
    let expected_parent = JournalSevenZipExtractIdentity {
        volume: parent_identity.volume,
        file_index: parent_identity.file_index,
    };
    #[cfg(test)]
    let cleanup_result = if held.close_uncertain_tree_cleanup {
        super::windows::extract_cleanup::delete_retained_spool_tree_root_close_uncertain_for_test(
            retained,
            &expected_parent,
            parent_path,
            cleanup_deadline,
        )
    } else if held.expire_tree_cleanup_after_identity {
        super::windows::extract_cleanup::delete_retained_spool_tree_expire_after_root_identity_for_test(
            retained,
            &expected_parent,
            parent_path,
            cleanup_deadline,
        )
    } else {
        delete_retained_spool_tree(retained, &expected_parent, parent_path, cleanup_deadline)
    };
    #[cfg(not(test))]
    let cleanup_result =
        delete_retained_spool_tree(retained, &expected_parent, parent_path, cleanup_deadline);
    let mut cleanup = match cleanup_result {
        Ok(()) => None,
        Err(failure) => Some(failure),
    };
    while let Some(failure) = cleanup.take() {
        if failure.reason != "staged child identity must be recorded before cleanup"
            || ledger.phase == Phase::PublishIntent
            || ledger.tree_state != ObjectState::DeleteIntent
            || !ledger.aborting
            || Instant::now() >= cleanup_deadline
        {
            cleanup = Some(failure);
            break;
        }
        if !cleanup_resume_controls_allow(controls, held, cleanup_deadline) {
            cleanup = Some(failure);
            break;
        }
        cleanup = match resume_staged_tree_delete(
            failure,
            &expected_parent,
            parent_path,
            cleanup_deadline,
        ) {
            Ok(()) => None,
            Err(failure) => Some(failure),
        };
    }
    if let Some(failure) = cleanup {
        held._staged_cleanup.push(failure);
        held.uncertain = true;
        return Err(WorkerFailure::Unknown(
            "partial staging tree cleanup did not confirm exact identity absence".into(),
        ));
    }
    advance(
        batch,
        ledger,
        ledger.phase,
        ObjectState::Owned,
        Some(spool_identity.clone()),
        ObjectState::Deleted,
        Some(tree_identity.clone()),
        None,
        None,
    )?;
    #[cfg(test)]
    if let Some(seam) = tree_cleanup_replay {
        // native tree cleanup が終わり、Tree=Deleted の後続snapshotを送る直前で停止する。
        seam.native_cleanup_finished.wait();
        seam.release_successor.wait();
        submit_batch_observing_unmatched(
            token,
            batch,
            controls,
            emit,
            held,
            seam.unmatched_control,
        )?;
    } else {
        submit_recording_successor(token, batch, controls, emit, held)?;
    }
    #[cfg(not(test))]
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        ledger.phase,
        ObjectState::DeleteIntent,
        Some(spool_identity.clone()),
        ObjectState::Deleted,
        Some(tree_identity.clone()),
        None,
        None,
    )?;
    submit_batch(
        token,
        batch,
        controls,
        emit,
        held,
        true,
        Some((
            ItemId(ordinal as u32),
            super::super::runtime::SevenZipExtractCleanupTarget::Spool,
        )),
    )?;
    // 正確なspool cleanup ACK後、decode/materializerの複製handleはすべてdrain済み。
    held._files.pop();
    if let Err(failure) = super::windows::extract_cleanup::delete_spool(
        spool,
        &spool_identity,
        parent_path,
        &expected_parent,
        cleanup_deadline,
    ) {
        held._cleanup.push(failure);
        held.uncertain = true;
        return Err(WorkerFailure::Unknown(
            "spool cleanup after materialization failure is unresolved".into(),
        ));
    }
    advance(
        batch,
        ledger,
        ledger.phase,
        ObjectState::Deleted,
        Some(spool_identity.clone()),
        ObjectState::Deleted,
        Some(tree_identity.clone()),
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        Phase::Discarded,
        ObjectState::Deleted,
        Some(spool_identity),
        ObjectState::Deleted,
        Some(tree_identity),
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    held.uncertain = false;
    held.receipts[ordinal] = Some(extract_item_receipt(
        ordinal,
        if cancelled.load(Ordering::Acquire) {
            ItemOutcome::CancelledBeforeStart
        } else {
            ItemOutcome::Failed
        },
        None,
        Some("7z spool-to-tree materialization failed; unpublished objects were discarded"),
    ));
    Err(if cancelled.load(Ordering::Acquire) {
        WorkerFailure::Cancelled
    } else {
        WorkerFailure::Rejected("7z spool-to-tree materialization failed")
    })
}

fn cleanup_resume_controls_allow(
    controls: &Receiver<WorkerControl>,
    held: &mut HeldCustody,
    deadline: Instant,
) -> bool {
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        match controls.try_recv() {
            Ok(WorkerControl::Cancel) => held.cancelled = true,
            Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Instant::now() < deadline,
        }
    }
}

fn abort_owned_spool_without_tree(
    token: AttemptToken,
    ordinal: usize,
    spool: File,
    parent_identity: &Identity,
    parent_path: &std::path::Path,
    batch: &mut Batch,
    ledger: &mut Ledger,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    cancelled: bool,
) -> Result<(), WorkerFailure> {
    let cleanup_deadline = Instant::now() + Duration::from_secs(30);
    let spool_identity = ledger.spool_identity.clone().ok_or_else(|| {
        WorkerFailure::Unknown("spool identity missing during cancellation cleanup".into())
    })?;
    advance_abort_latch(batch, ledger)?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    if ledger.tree_state != ObjectState::NeverCreated {
        advance(
            batch,
            ledger,
            ledger.phase,
            ledger.spool_state,
            Some(spool_identity.clone()),
            ObjectState::NeverCreated,
            None,
            None,
            None,
        )?;
        submit_recording_successor(token, batch, controls, emit, held)?;
    }
    advance(
        batch,
        ledger,
        ledger.phase,
        ObjectState::DeleteIntent,
        Some(spool_identity.clone()),
        ObjectState::NeverCreated,
        None,
        None,
        None,
    )?;
    submit_batch(
        token,
        batch,
        controls,
        emit,
        held,
        true,
        Some((
            ItemId(ordinal as u32),
            super::super::runtime::SevenZipExtractCleanupTarget::Spool,
        )),
    )?;
    held._files.pop();
    if let Err(failure) = super::windows::extract_cleanup::delete_spool(
        spool,
        &spool_identity,
        parent_path,
        &JournalSevenZipExtractIdentity {
            volume: parent_identity.volume,
            file_index: parent_identity.file_index,
        },
        cleanup_deadline,
    ) {
        held._cleanup.push(failure);
        held.uncertain = true;
        return Err(WorkerFailure::Unknown(
            "cancelled spool cleanup did not confirm exact identity absence".into(),
        ));
    }
    advance(
        batch,
        ledger,
        ledger.phase,
        ObjectState::Deleted,
        Some(spool_identity.clone()),
        ObjectState::NeverCreated,
        None,
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    advance(
        batch,
        ledger,
        Phase::Discarded,
        ObjectState::Deleted,
        Some(spool_identity),
        ObjectState::NeverCreated,
        None,
        None,
        None,
    )?;
    submit_recording_successor(token, batch, controls, emit, held)?;
    held.receipts[ordinal] = Some(extract_item_receipt(
        ordinal,
        if cancelled {
            ItemOutcome::CancelledBeforeStart
        } else {
            ItemOutcome::Failed
        },
        None,
        Some(if cancelled {
            "7z extraction was cancelled before tree creation"
        } else {
            "7z extraction stopped before tree creation"
        }),
    ));
    held.uncertain = false;
    Ok(())
}

fn submit_batch(
    token: AttemptToken,
    batch: &Batch,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    cleanup: bool,
    target: Option<(ItemId, super::super::runtime::SevenZipExtractCleanupTarget)>,
) -> Result<(), WorkerFailure> {
    submit_batch_inner(
        token,
        batch,
        controls,
        emit,
        held,
        cleanup,
        target,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
fn submit_batch_observing_unmatched(
    token: AttemptToken,
    batch: &Batch,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    unmatched_control: std::sync::mpsc::Sender<WorkerControl>,
) -> Result<(), WorkerFailure> {
    match submit_batch_inner(
        token,
        batch,
        controls,
        emit,
        held,
        false,
        None,
        Some(unmatched_control),
    ) {
        Ok(()) | Err(WorkerFailure::Observed(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

fn submit_batch_inner(
    token: AttemptToken,
    batch: &Batch,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
    cleanup: bool,
    target: Option<(ItemId, super::super::runtime::SevenZipExtractCleanupTarget)>,
    #[cfg(test)] unmatched_observer: Option<std::sync::mpsc::Sender<WorkerControl>>,
) -> Result<(), WorkerFailure> {
    if batch.validate().is_err() {
        return Err(WorkerFailure::Unknown(
            "7z worker produced an invalid ledger snapshot".into(),
        ));
    }
    held.last_batch = Some(batch.clone());
    emit(ExecutorFact::SevenZipExtractBatchIntent {
        token,
        batch: batch.clone(),
    });
    loop {
        match controls.recv() {
            Ok(WorkerControl::Cancel) => {
                held.cancelled = true;
                continue;
            }
            Ok(WorkerControl::SevenZipExtractBatchJournaledExact {
                token: ack,
                batch: accepted,
            }) if !cleanup && ack == token && accepted == *batch => return Ok(()),
            Ok(WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                token: ack,
                batch: accepted,
                item,
                target: ack_target,
            }) if cleanup
                && ack == token
                && accepted == *batch
                && target == Some((ItemId(item.0), ack_target)) =>
            {
                return Ok(());
            }
            Ok(WorkerControl::SevenZipExtractBatchObservedExact {
                token: ack,
                batch: observed,
            }) if ack == token && observed == *batch => {
                held.uncertain = true;
                return Err(WorkerFailure::Observed(
                    "7z ledger was observed after mutation permission was revoked".into(),
                ));
            }
            Ok(WorkerControl::SevenZipExtractBatchJournalFailedExact {
                token: ack,
                batch: failed,
            }) if ack == token && failed == *batch => {
                return Err(WorkerFailure::Unknown(
                    "7z extraction journal write failed".into(),
                ));
            }
            Ok(unmatched) => {
                #[cfg(test)]
                if let Some(observer) = &unmatched_observer {
                    observer.send(unmatched.clone()).map_err(|_| {
                        WorkerFailure::Unknown("test unmatched-control observer closed".into())
                    })?;
                }
                #[cfg(not(test))]
                let _ = unmatched;
                continue;
            }
            Err(_) => {
                return Err(WorkerFailure::Unknown(
                    "7z extraction runtime control channel closed".into(),
                ));
            }
        }
    }
}

fn submit_recording_successor(
    token: AttemptToken,
    batch: &Batch,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
    held: &mut HeldCustody,
) -> Result<(), WorkerFailure> {
    match submit_batch(token, batch, controls, emit, held, false, None) {
        Ok(()) | Err(WorkerFailure::Observed(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn advance(
    batch: &mut Batch,
    ledger: &mut Ledger,
    phase: Phase,
    spool_state: ObjectState,
    spool_identity: Option<Identity>,
    tree_state: ObjectState,
    tree_identity: Option<Identity>,
    spool_ready: Option<SpoolReady>,
    tree_ready: Option<TreeReady>,
) -> Result<(), WorkerFailure> {
    // 呼出側で次の観測事実を先に組み立てた場合も、比較基準は最後に受理された snapshot。
    let previous = batch
        .items
        .last()
        .cloned()
        .ok_or_else(|| WorkerFailure::Unknown("7z ledger item disappeared".into()))?;
    let previous_batch = batch.clone();
    ledger.sequence = ledger
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerFailure::Unknown("7z ledger sequence exhausted".into()))?;
    batch.sequence = batch
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerFailure::Unknown("7z batch sequence exhausted".into()))?;
    ledger.phase = phase;
    ledger.spool_state = spool_state;
    ledger.spool_identity = spool_identity;
    ledger.tree_state = tree_state;
    ledger.tree_identity = tree_identity;
    if spool_ready.is_some() {
        ledger.spool_ready = spool_ready;
    }
    if tree_ready.is_some() {
        ledger.tree_ready = tree_ready;
    }
    if ledger.validate().is_err() || !ledger.follows(&previous) {
        return Err(WorkerFailure::Unknown(
            "7z ledger successor validation failed".into(),
        ));
    }
    *batch
        .items
        .last_mut()
        .ok_or_else(|| WorkerFailure::Unknown("7z ledger item disappeared".into()))? =
        ledger.clone();
    if batch.validate().is_err() || !batch.follows(&previous_batch) {
        return Err(WorkerFailure::Unknown(
            "7z batch successor validation failed".into(),
        ));
    }
    Ok(())
}

fn advance_abort_latch(batch: &mut Batch, ledger: &mut Ledger) -> Result<(), WorkerFailure> {
    let previous = batch
        .items
        .last()
        .cloned()
        .ok_or_else(|| WorkerFailure::Unknown("7z ledger item disappeared".into()))?;
    let previous_batch = batch.clone();
    let mut next = previous.clone();
    next.sequence = next
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerFailure::Unknown("7z ledger sequence exhausted".into()))?;
    next.aborting = true;
    let mut next_batch = batch.clone();
    next_batch.sequence = next_batch
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerFailure::Unknown("7z batch sequence exhausted".into()))?;
    if !next.follows(&previous) {
        return Err(WorkerFailure::Unknown(
            "7z abort latch is not a legal ledger successor".into(),
        ));
    }
    *next_batch
        .items
        .last_mut()
        .ok_or_else(|| WorkerFailure::Unknown("7z ledger item disappeared".into()))? = next.clone();
    if next_batch.validate().is_err() || !next_batch.follows(&previous_batch) {
        return Err(WorkerFailure::Unknown(
            "7z abort latch batch validation failed".into(),
        ));
    }
    *ledger = next;
    *batch = next_batch;
    Ok(())
}

fn append_initial_ledger(batch: &mut Batch, ledger: &Ledger) -> Result<(), WorkerFailure> {
    let previous = batch.clone();
    if !ledger.is_initial() {
        return Err(WorkerFailure::Unknown(
            "7z item registration is not an initial ledger".into(),
        ));
    }
    let mut next = previous.clone();
    next.sequence = next
        .sequence
        .checked_add(1)
        .ok_or_else(|| WorkerFailure::Unknown("7z batch sequence exhausted".into()))?;
    next.items.push(ledger.clone());
    if let Err(error) = next.validate() {
        return Err(WorkerFailure::Unknown(format!(
            "7z item registration batch is invalid: {error}"
        )));
    }
    if !next.follows(&previous) {
        return Err(WorkerFailure::Unknown(
            "7z item registration is not a legal batch successor".into(),
        ));
    }
    *batch = next;
    Ok(())
}

fn build_descriptors(
    request: &super::super::model::OperationRequest,
    attempt: &AttemptId,
) -> Result<Vec<Descriptor>, WorkerFailure> {
    let mut result = Vec::with_capacity(request.sources.len());
    for (ordinal, source) in request.sources.iter().enumerate() {
        let parent = source
            .parent()
            .ok_or(WorkerFailure::Rejected("7z source has no parent"))?;
        let output = parent.join(
            source
                .file_stem()
                .ok_or(WorkerFailure::Rejected("7z source has no output stem"))?,
        );
        let nonce = super::TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let suffix = format!("{}-{}-{}-{ordinal}", std::process::id(), attempt.job, nonce);
        result.push(Descriptor {
            ordinal: ordinal as u32,
            source_path: JournalPath::from_path(source),
            source_identity: identity_from_fs(
                request
                    .seven_zip_extract_guard
                    .as_ref()
                    .ok_or(WorkerFailure::Rejected("7z guard is missing"))?
                    .sources[ordinal]
                    .source_identity
                    .clone(),
            )?,
            output: JournalPath::from_path(&output),
            spool: JournalPath::from_path(&parent.join(format!(".gpui-filer-7z-spool-{suffix}"))),
            tree: JournalPath::from_path(&parent.join(format!(".gpui-filer-7z-tree-{suffix}"))),
        });
    }
    Ok(result)
}

fn file_binding(hint: &crate::archive_provider::ArchiveFileHint) -> FileBinding {
    FileBinding {
        path: JournalPath::from_path(&hint.path),
        identity: Identity {
            volume: hint.identity.volume_serial,
            file_index: hint.identity.file_index,
        },
    }
}
fn path_from_journal(path: &JournalPath) -> Result<PathBuf, WorkerFailure> {
    use std::os::windows::ffi::OsStringExt;
    if path.units.is_empty() || path.units.contains(&0) {
        return Err(WorkerFailure::Rejected(
            "journal path cannot be represented",
        ));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&path.units)))
}
fn identity_from_fs(
    value: crate::filesystem_identity::FilesystemIdentity,
) -> Result<Identity, WorkerFailure> {
    let (volume, file_index) = value
        .windows_parts()
        .ok_or(WorkerFailure::Rejected("7z filesystem identity is invalid"))?;
    Ok(Identity { volume, file_index })
}
fn file_identity(file: &File) -> Result<Identity, WorkerFailure> {
    let value = super::windows::regular_file_identity(file).map_err(|_| {
        WorkerFailure::Unknown("created spool identity could not be observed".into())
    })?;
    Ok(Identity {
        volume: value.volume,
        file_index: (u64::from(value.index_high) << 32) | u64::from(value.index_low),
    })
}
fn identity_from_text(value: &str) -> Result<Identity, WorkerFailure> {
    let (volume, index) = value
        .split_once(':')
        .ok_or_else(|| WorkerFailure::Unknown("created tree identity is malformed".into()))?;
    Ok(Identity {
        volume: volume.parse().map_err(|_| {
            WorkerFailure::Unknown("created tree volume identity is malformed".into())
        })?,
        file_index: u64::from_str_radix(index, 16).map_err(|_| {
            WorkerFailure::Unknown("created tree file identity is malformed".into())
        })?,
    })
}
fn summarize_plan(plan: &Validated7zExtractPlan) -> Result<PlanSummary, WorkerFailure> {
    let file_count = u32::try_from(
        plan.entries()
            .iter()
            .filter(|entry| entry.kind() == crate::archive_manifest::NodeKind::File)
            .count(),
    )
    .map_err(|_| WorkerFailure::Rejected("7z plan file count exceeds its bound"))?;
    Ok(PlanSummary {
        plan_format: 1,
        sha256: crate::archive_provider::ArchiveProviderOwner::seven_zip_extract_plan_digest(plan)
            .map_err(|_| WorkerFailure::Rejected("7z canonical plan digest failed"))?,
        indexed_record_count: u32::try_from(plan.entries().len())
            .map_err(|_| WorkerFailure::Rejected("7z plan record count exceeds its bound"))?,
        materialized_node_count: u32::try_from(plan.nodes().len())
            .map_err(|_| WorkerFailure::Rejected("7z plan node count exceeds its bound"))?,
        file_count,
        total_expanded_bytes: plan.total_size(),
    })
}
fn last_write_ticks(metadata: &std::fs::Metadata) -> Result<u64, WorkerFailure> {
    let modified = metadata
        .modified()
        .map_err(|_| WorkerFailure::Rejected("7z source timestamp could not be read"))?;
    let duration = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| WorkerFailure::Rejected("7z source timestamp is invalid"))?;
    duration
        .as_nanos()
        .checked_div(100)
        .and_then(|ticks| u64::try_from(ticks).ok())
        .ok_or(WorkerFailure::Rejected("7z source timestamp overflow"))
}

fn verify_source_signature(file: &File, expected: &SourceFact) -> Result<(), WorkerFailure> {
    let identity = file_identity(file)?;
    let metadata = file
        .metadata()
        .map_err(|_| WorkerFailure::Rejected("7z source signature could not be rechecked"))?;
    if identity != expected.identity
        || metadata.len() != expected.length
        || last_write_ticks(&metadata)? != expected.last_write_ticks
    {
        return Err(WorkerFailure::Rejected(
            "7z source changed between inspection and decode",
        ));
    }
    Ok(())
}
fn create_spool(path: &std::path::Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(
            ::windows::Win32::Foundation::GENERIC_READ.0
                | ::windows::Win32::Foundation::GENERIC_WRITE.0
                | ::windows::Win32::Storage::FileSystem::DELETE.0,
        )
        .share_mode(::windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
        .open(path)
}
fn check(
    cancelled: &AtomicBool,
    deadline: Instant,
    held: &HeldCustody,
) -> Result<(), WorkerFailure> {
    if cancelled.load(Ordering::Acquire) || held.cancelled {
        return Err(WorkerFailure::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(WorkerFailure::Rejected(
            "7z extraction work deadline expired",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn cleanup_resume_latches_cancel_but_rejects_owner_loss_before_native_retry() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut held = HeldCustody::default();
        sender.send(WorkerControl::Cancel).unwrap();
        drop(sender);

        assert!(!cleanup_resume_controls_allow(
            &receiver,
            &mut held,
            Instant::now() + Duration::from_secs(1),
        ));
        assert!(held.cancelled);
    }

    #[test]
    fn legal_builder_sequences_append_publish_and_abort_as_separate_observations() {
        let mut spool_owned = crate::operations::journal::test_fixture();
        spool_owned.binding.item = 0;
        let jpath = |value: &str| JournalPath {
            units: value.encode_utf16().collect(),
        };
        spool_owned.binding.source.path = jpath(r"D:\archive.7z");
        spool_owned.binding.output = jpath(r"D:\archive");
        let binding = spool_owned.binding.clone();
        let descriptor = Descriptor {
            ordinal: binding.item,
            source_path: binding.source.path.clone(),
            source_identity: binding.source.identity.clone(),
            output: binding.output.clone(),
            spool: binding.spool.clone(),
            tree: binding.tree.clone(),
        };
        let envelope = Envelope {
            attempt: binding.attempt.clone(),
            reservation_format: SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
            provider_revision: binding.provider_revision,
            provider_executable: binding.provider_executable.clone(),
            provider_dll: binding.provider_dll.clone(),
            application_image: binding.application_image.clone(),
            destination_parent: binding.destination_parent.clone(),
            items: vec![descriptor],
        };
        let mut batch = Batch {
            envelope: envelope.clone(),
            sequence: 1,
            items: Vec::new(),
        };
        append_initial_ledger(&mut batch, &spool_owned).unwrap();
        assert_eq!(batch.sequence, 2);
        assert_eq!(spool_owned.sequence, 1);

        let identity = Identity {
            volume: binding.destination_parent.volume,
            file_index: binding.destination_parent.file_index + 10,
        };
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::SpoolOwned,
            ObjectState::Owned,
            Some(identity.clone()),
            ObjectState::Planned,
            None,
            None,
            None,
        )
        .unwrap();
        let old = batch.clone();
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::SpoolReady,
            ObjectState::Owned,
            Some(identity.clone()),
            ObjectState::Planned,
            None,
            Some(SpoolReady {
                plan_format: binding.plan.plan_format,
                plan_sha256: binding.plan.sha256,
                length: binding.plan.total_expanded_bytes,
            }),
            None,
        )
        .unwrap();
        assert!(batch.follows(&old));

        advance(
            &mut batch,
            &mut spool_owned,
            Phase::TreeCreateIntent,
            ObjectState::Owned,
            Some(identity.clone()),
            ObjectState::CreateIntent,
            None,
            None,
            None,
        )
        .unwrap();
        let tree_id = Identity {
            file_index: identity.file_index + 1,
            ..identity.clone()
        };
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::TreeOwned,
            ObjectState::Owned,
            Some(identity.clone()),
            ObjectState::Owned,
            Some(tree_id.clone()),
            None,
            None,
        )
        .unwrap();
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::TreeReady,
            ObjectState::Owned,
            Some(identity.clone()),
            ObjectState::Owned,
            Some(tree_id.clone()),
            None,
            Some(TreeReady {
                plan_format: binding.plan.plan_format,
                plan_sha256: binding.plan.sha256,
                materialized_node_count: binding.plan.materialized_node_count,
                file_count: binding.plan.file_count,
                total_verified_bytes: binding.plan.total_expanded_bytes,
            }),
        )
        .unwrap();
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::TreeReady,
            ObjectState::DeleteIntent,
            Some(identity.clone()),
            ObjectState::Owned,
            Some(tree_id.clone()),
            None,
            None,
        )
        .unwrap();
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::SpoolCleaned,
            ObjectState::Deleted,
            Some(identity.clone()),
            ObjectState::Owned,
            Some(tree_id.clone()),
            None,
            None,
        )
        .unwrap();
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::PublishIntent,
            ObjectState::Deleted,
            Some(identity.clone()),
            ObjectState::Owned,
            Some(tree_id.clone()),
            None,
            None,
        )
        .unwrap();
        spool_owned.publication_identity = Some(tree_id.clone());
        advance(
            &mut batch,
            &mut spool_owned,
            Phase::Published,
            ObjectState::Deleted,
            Some(identity),
            ObjectState::Published,
            Some(tree_id),
            None,
            None,
        )
        .unwrap();

        let mut abort = crate::operations::journal::test_fixture();
        abort.binding.item = 0;
        abort.binding.source.path = jpath(r"D:\archive.7z");
        abort.binding.output = jpath(r"D:\archive");
        let abort_binding = abort.binding.clone();
        let mut abort_batch = Batch {
            envelope,
            sequence: 1,
            items: Vec::new(),
        };
        append_initial_ledger(&mut abort_batch, &abort).unwrap();
        let spool_id = Identity {
            volume: abort_binding.destination_parent.volume,
            file_index: abort_binding.destination_parent.file_index + 20,
        };
        advance(
            &mut abort_batch,
            &mut abort,
            Phase::SpoolOwned,
            ObjectState::Owned,
            Some(spool_id.clone()),
            ObjectState::Planned,
            None,
            None,
            None,
        )
        .unwrap();
        let previous = abort_batch.clone();
        advance_abort_latch(&mut abort_batch, &mut abort).unwrap();
        assert!(abort_batch.follows(&previous));
        let mut skipped_batch = abort_batch.clone();
        let mut skipped_ledger = abort.clone();
        assert!(
            advance(
                &mut skipped_batch,
                &mut skipped_ledger,
                Phase::Discarded,
                ObjectState::Deleted,
                Some(spool_id.clone()),
                ObjectState::NeverCreated,
                None,
                None,
                None,
            )
            .is_err()
        );
        let previous = abort_batch.clone();
        advance(
            &mut abort_batch,
            &mut abort,
            Phase::SpoolOwned,
            ObjectState::DeleteIntent,
            Some(spool_id.clone()),
            ObjectState::Planned,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(abort_batch.follows(&previous));
        advance(
            &mut abort_batch,
            &mut abort,
            Phase::SpoolOwned,
            ObjectState::Deleted,
            Some(spool_id.clone()),
            ObjectState::Planned,
            None,
            None,
            None,
        )
        .unwrap();
        advance(
            &mut abort_batch,
            &mut abort,
            Phase::SpoolOwned,
            ObjectState::Deleted,
            Some(spool_id.clone()),
            ObjectState::NeverCreated,
            None,
            None,
            None,
        )
        .unwrap();
        advance(
            &mut abort_batch,
            &mut abort,
            Phase::Discarded,
            ObjectState::Deleted,
            Some(spool_id),
            ObjectState::NeverCreated,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(abort_batch.validate().is_ok());
    }

    #[test]
    fn held_parent_route_blocks_rename_until_custody_is_released() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("destination-parent");
        let moved = temp.path().join("destination-parent-moved");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("source.7z");
        std::fs::write(&source, b"fixture").unwrap();
        let expected =
            crate::operations::model::test_zip_create_guard(&source, &parent.join("unused-output"))
                .destination_parent_identity;
        let initial = super::super::windows::lock_virtual_destination(&parent, None).unwrap();
        drop(initial);

        let mut held = HeldCustody::default();
        held._parent_route =
            super::super::windows::lock_virtual_destination(&parent, Some(&expected)).unwrap();
        held.uncertain = true; // モックの終端永続化待ち中も worker custody が route を保持する。

        assert!(std::fs::rename(&parent, &moved).is_err());
        assert!(parent.is_dir());
        drop(held);
        std::fs::rename(&parent, &moved).unwrap();
        assert!(moved.is_dir());
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_mixed_fixture_persists_terminal_and_exact_tree() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_implicit_parent_fixture_persists_terminal_and_exact_tree() {
        run_real_worker_success_fixture(
            "implicit", false, false, false, false, false, false, false, false, false, false,
            false, false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_empty_archive_fixture_persists_terminal_and_exact_tree() {
        run_real_worker_success_fixture(
            "empty", false, false, false, false, false, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_published_write_failure_retains_tree_until_repair_and_terminal_release() {
        run_real_worker_success_fixture(
            "mixed", true, false, false, false, false, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_materialize_failure_resumes_unknown_children_under_one_tree_intent() {
        run_real_worker_success_fixture(
            "mixed", false, true, false, false, false, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_rejects_replayed_tree_cleanup_ack_before_spool_cleanup() {
        run_real_worker_fixture_with_drain(
            "mixed", false, true, false, false, false, false, false, false, false, false, false,
            false, false, false, None, false, true,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_expired_deadline_after_publish_ack_keeps_stage_unpublished() {
        run_real_worker_success_fixture(
            "mixed", false, false, true, false, false, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_spool_create_collision_preserves_foreign_sentinel() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, true, false, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_tree_create_collision_preserves_foreign_sentinel() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, true, false, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_initial_tree_identity_failure_retains_created_directory_until_terminal_ack() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, true, false, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_expired_deadline_after_tree_intent_cleans_spool_before_root_create() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, true, false, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_expired_deadline_after_decode_drains_then_cleans_owned_spool() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, true, false, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_expired_deadline_after_first_child_write_cleans_partial_tree() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, true, false, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_close_uncertain_tree_cleanup_keeps_custody_until_terminal_release() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, false, true, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_cancel_after_publish_intent_keeps_unpublished_stage_unknown() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, false, true, false,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_publish_collision_after_intent_keeps_stage_and_foreign_tree() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, false, false, true,
            false, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_post_rename_root_reopen_failure_keeps_published_tree_unknown() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, false, false, false,
            true, false, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_cleanup_deadline_expiry_keeps_partial_tree_unknown() {
        run_real_worker_success_fixture(
            "mixed", false, false, false, false, false, false, false, false, false, false, false,
            false, false, true,
        );
    }

    #[test]
    #[ignore = "requires fresh release and native Windows process/job support"]
    fn real_worker_cancelled_decode_waits_for_descendant_drain_before_spool_cleanup() {
        run_real_worker_fixture_with_drain(
            "mixed",
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            Some(DecodeDrainFixture::Cancel),
            false,
            false,
        );
    }

    #[test]
    #[ignore = "requires fresh release and native Windows process/job support"]
    fn real_worker_decode_error_waits_for_descendant_exit_before_spool_cleanup() {
        run_real_worker_fixture_with_drain(
            "mixed",
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            Some(DecodeDrainFixture::Error),
            false,
            false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_rejects_source_replacement_during_registration_ack_wait() {
        run_real_worker_fixture_with_drain(
            "mixed", false, false, false, false, false, false, false, false, false, false, false,
            false, false, false, None, true, false,
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_rejects_stale_provider_before_registration() {
        run_real_worker_stale_provider_fixture();
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_stale_cancel_from_terminal_attempt_does_not_cancel_fresh_extract() {
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("mixed.7z");
        let fixture_archive = fixture_root.join("mixed.7z");
        let fixture_expected = fixture_root.join("mixed-source");
        assert!(release.is_file() && seven_zip.is_file() && fixture_archive.is_file());
        let source_bytes = std::fs::read(&fixture_archive).unwrap();
        std::fs::write(&source, &source_bytes).unwrap();
        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .expect("installed 7-Zip discovery");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .unwrap();
        assert!(provider.commit_discovery(revision, Some(executable)));
        let guard = crate::operations::model::test_zip_create_guard(
            &source,
            &temp.path().join("unused-output"),
        );
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            vec![source.clone()],
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }],
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        let stale_image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease for cancelled attempt");
        let fresh_image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease for fresh attempt");
        let journal_path = temp.path().join("stale-cancel-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();

        let stale_token = runtime.submit(request.clone()).unwrap();
        let stale_start = runtime.start_next().unwrap();
        let stale_attempt = stale_start.seven_zip_extract_attempt.unwrap();
        let stale_cancel = stale_start.cancellation.clone();
        let stale_worker_cancel = stale_cancel.clone();
        let stale_worker_attempt = stale_attempt.clone();
        let stale_controls = stale_start.controls;
        assert!(runtime.cancel(stale_token).is_none());
        assert!(stale_cancel.load(Ordering::Acquire));
        let stale_provider = provider.clone();
        let stale_request = request.clone();
        let (stale_fact_tx, stale_fact_rx) = std::sync::mpsc::channel();
        let stale_worker = std::thread::spawn(move || {
            super::execute(
                stale_token,
                &stale_request,
                Some(stale_worker_attempt),
                Some(stale_provider),
                Some(stale_image),
                &stale_worker_cancel,
                &stale_controls,
                &mut |fact| stale_fact_tx.send(fact).unwrap(),
            );
        });
        let stale_report = match stale_fact_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("timed out waiting for cancelled attempt terminal fact")
        {
            ExecutorFact::Terminal { token, report } if token == stale_token => report,
            fact => panic!("cancelled attempt emitted unexpected first fact: {fact:?}"),
        };
        assert_eq!(
            stale_report.outcome,
            crate::operations::model::TerminalOutcome::Cancelled
        );
        runtime.commit_fact(ExecutorFact::Terminal {
            token: stale_token,
            report: stale_report,
        });
        let stale_terminal_projection = runtime.job(stale_token).unwrap();
        let stale_terminal_projection = stale_terminal_projection
            .attempts
            .iter()
            .find(|attempt| attempt.token == stale_token)
            .unwrap();
        assert_eq!(
            stale_terminal_projection.state,
            crate::operations::model::AttemptState::Terminal
        );
        let stale_terminal_report = stale_terminal_projection.terminal.clone().unwrap();
        let (predecessors, stale_terminal) = runtime
            .seven_zip_extract_terminal_write_plan(stale_token)
            .expect("cancelled attempt terminal write plan");
        assert!(predecessors.is_empty());
        writer
            .upsert_with_reply(stale_terminal.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        assert!(
            runtime.acknowledge_seven_zip_extract_terminal_record(stale_token, &stale_terminal)
        );
        stale_worker.join().unwrap();
        let stale_terminal_snapshot = serde_json::to_value(&stale_terminal).unwrap();
        let assert_stale_terminal_snapshot_unchanged = || {
            let persisted: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
            assert!(
                persisted["attempts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|attempt| attempt == &stale_terminal_snapshot)
            );
        };
        let stale_projection = runtime.job(stale_token).unwrap();
        let stale_projection = stale_projection
            .attempts
            .iter()
            .find(|attempt| attempt.token == stale_token)
            .unwrap();
        assert_eq!(
            stale_projection.state,
            crate::operations::model::AttemptState::Terminal
        );
        assert_eq!(
            stale_projection.terminal.as_ref(),
            Some(&stale_terminal_report)
        );
        assert_stale_terminal_snapshot_unchanged();

        let fresh_token = runtime.submit(request.clone()).unwrap();
        let fresh_start = runtime
            .start_next()
            .expect("fresh explicit extraction starts");
        let fresh_attempt = fresh_start.seven_zip_extract_attempt.unwrap();
        assert_ne!(stale_token, fresh_token);
        assert_ne!(stale_attempt, fresh_attempt);
        assert!(!std::sync::Arc::ptr_eq(
            &stale_cancel,
            &fresh_start.cancellation
        ));
        let fresh_cancel = fresh_start.cancellation.clone();
        let fresh_worker_cancel = fresh_cancel.clone();
        let (worker_control_tx, worker_control_rx) = std::sync::mpsc::channel();
        let (observed_control_tx, observed_control_rx) = std::sync::mpsc::channel();
        let runtime_controls = fresh_start.controls;
        let control_bridge = std::thread::spawn(move || {
            loop {
                let control = match runtime_controls.recv_timeout(Duration::from_secs(120)) {
                    Ok(control) => control,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        panic!("timed out waiting for a fresh-attempt worker control")
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if observed_control_tx.send(control.clone()).is_err()
                    || worker_control_tx.send(control).is_err()
                {
                    break;
                }
            }
        });
        let fresh_provider = provider.clone();
        let fresh_request = request.clone();
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute(
                fresh_token,
                &fresh_request,
                Some(fresh_attempt),
                Some(fresh_provider),
                Some(fresh_image),
                &fresh_worker_cancel,
                &worker_control_rx,
                &mut |fact| fact_tx.send(fact).unwrap(),
            );
        });
        let receive_worker_fact = || {
            fact_rx
                .recv_timeout(Duration::from_secs(120))
                .expect("timed out waiting for a fresh-worker fact")
        };
        let mut saw_spool_intent = false;
        let mut exact_batches = 0usize;
        let mut spool_path = None;
        let mut tree_path = None;
        let mut terminal = None;
        while terminal.is_none() {
            match receive_worker_fact() {
                ExecutorFact::Progress { token, update } => {
                    assert_eq!(token, fresh_token);
                    runtime.commit_progress(fresh_token, &update);
                }
                ExecutorFact::SevenZipExtractBatchIntent { token, batch } => {
                    assert_eq!(token, fresh_token);
                    runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
                        token,
                        batch: batch.clone(),
                    });
                    let record = runtime
                        .claim_pending_seven_zip_extract_batch_write(token, &batch)
                        .expect("fresh batch receives a serial write claim");
                    let expected_record = serde_json::to_value(&record).unwrap();
                    writer
                        .upsert_with_reply(record)
                        .recv_blocking()
                        .unwrap()
                        .unwrap();
                    let persisted: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
                    assert!(
                        persisted["attempts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|attempt| attempt == &expected_record),
                        "the exact fresh batch record is durable before its ACK"
                    );
                    if batch
                        .items
                        .iter()
                        .any(|item| item.phase == Phase::SpoolCreateIntent)
                    {
                        saw_spool_intent = true;
                        let descriptor = batch
                            .envelope
                            .items
                            .first()
                            .expect("fresh extraction has one spool descriptor");
                        spool_path = Some(super::path_from_journal(&descriptor.spool).unwrap());
                        tree_path = Some(super::path_from_journal(&descriptor.tree).unwrap());
                        assert!(runtime.cancel(stale_token).is_none());
                        let stale_projection = runtime.job(stale_token).unwrap();
                        let stale_projection = stale_projection
                            .attempts
                            .iter()
                            .find(|attempt| attempt.token == stale_token)
                            .unwrap();
                        assert_eq!(
                            stale_projection.state,
                            crate::operations::model::AttemptState::Terminal
                        );
                        assert_eq!(
                            stale_projection.terminal.as_ref(),
                            Some(&stale_terminal_report)
                        );
                        assert!(!fresh_cancel.load(Ordering::Acquire));
                        let active = runtime.active_job().unwrap();
                        assert_eq!(active.attempts.last().unwrap().token, fresh_token);
                        assert_eq!(
                            active.attempts.last().unwrap().state,
                            crate::operations::model::AttemptState::Running
                        );
                        assert_stale_terminal_snapshot_unchanged();
                    }
                    assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                    let control = observed_control_rx
                        .recv_timeout(Duration::from_secs(120))
                        .expect("timed out waiting for batch authorization to reach worker");
                    if batch
                        .items
                        .iter()
                        .any(|item| item.phase == Phase::SpoolCreateIntent)
                    {
                        assert!(matches!(
                            &control,
                            WorkerControl::SevenZipExtractBatchJournaledExact {
                                token: observed,
                                batch: exact,
                            } if *observed == fresh_token && exact == &batch
                        ));
                    }
                    assert!(
                        !matches!(
                            control,
                            WorkerControl::Cancel
                                | WorkerControl::SevenZipExtractBatchObservedExact { .. }
                        ),
                        "fresh attempt received stale cancellation or observed-only control: {control:?}"
                    );
                    let control_token = match &control {
                        WorkerControl::SevenZipExtractBatchJournaledExact { token, .. }
                        | WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                            token, ..
                        } => *token,
                        _ => panic!("unexpected fresh batch control: {control:?}"),
                    };
                    assert_eq!(control_token, fresh_token);
                    exact_batches += usize::from(matches!(
                        control,
                        WorkerControl::SevenZipExtractBatchJournaledExact { .. }
                    ));
                }
                ExecutorFact::Terminal { token, report } => {
                    assert_eq!(token, fresh_token);
                    assert_eq!(
                        report.outcome,
                        crate::operations::model::TerminalOutcome::Succeeded
                    );
                    runtime.commit_fact(ExecutorFact::Terminal { token, report });
                    let (predecessors, record) = runtime
                        .seven_zip_extract_terminal_write_plan(token)
                        .expect("fresh terminal write plan");
                    assert!(predecessors.is_empty());
                    writer
                        .upsert_with_reply(record.clone())
                        .recv_blocking()
                        .unwrap()
                        .unwrap();
                    assert!(runtime.acknowledge_seven_zip_extract_terminal_record(token, &record));
                    assert!(matches!(
                        observed_control_rx
                            .recv_timeout(Duration::from_secs(120))
                            .expect("timed out waiting for terminal ACK to reach worker"),
                        WorkerControl::SevenZipExtractTerminalJournaledExact { token: observed, .. }
                            if observed == fresh_token
                    ));
                    terminal = Some(record);
                }
                fact => panic!("unexpected fresh worker fact: {fact:?}"),
            }
        }
        assert!(saw_spool_intent);
        assert!(exact_batches > 0);
        worker.join().unwrap();
        let stale_projection = runtime.job(stale_token).unwrap();
        let stale_projection = stale_projection
            .attempts
            .iter()
            .find(|attempt| attempt.token == stale_token)
            .unwrap();
        assert_eq!(
            stale_projection.state,
            crate::operations::model::AttemptState::Terminal
        );
        assert_eq!(
            stale_projection.terminal.as_ref(),
            Some(&stale_terminal_report)
        );
        assert_stale_terminal_snapshot_unchanged();
        drop(runtime);
        control_bridge.join().unwrap();
        drop(writer);

        let output = temp.path().join("mixed");
        assert_eq!(tree_snapshot(&output), tree_snapshot(&fixture_expected));
        assert!(!spool_path.unwrap().exists(), "owned spool is cleaned");
        assert!(
            !tree_path.unwrap().exists(),
            "staging tree is published or cleaned"
        );
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        let recovery = crate::operations::journal::JournalStore::open(journal_path).unwrap();
        assert!(recovery.recoveries().is_empty());
    }

    fn run_real_worker_stale_provider_fixture() {
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        let fixture_archive = fixture_root.join("mixed.7z");
        assert!(release.is_file() && seven_zip.is_file() && fixture_archive.is_file());
        let source_bytes = std::fs::read(&fixture_archive).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("mixed.7z");
        std::fs::write(&source, &source_bytes).unwrap();
        let output = temp.path().join("mixed");
        let destination_sentinel = b"foreign destination sentinel";
        std::fs::write(&output, destination_sentinel).unwrap();
        let before_namespace = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();

        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .expect("installed 7-Zip discovery");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .expect("read-only operation binding");
        assert!(provider.commit_discovery(revision, Some(executable)));
        let guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            vec![source.clone()],
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }],
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        assert_eq!(
            request
                .seven_zip_extract_guard
                .as_ref()
                .unwrap()
                .provider_revision,
            revision
        );

        let advanced_revision = provider.begin_discovery();
        assert_eq!(advanced_revision, revision + 1);
        assert!(
            advanced_revision
                > request
                    .seven_zip_extract_guard
                    .as_ref()
                    .unwrap()
                    .provider_revision
        );

        let image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease");
        let journal_path = temp.path().join("stale-provider-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime
            .start_next()
            .expect("runtime admits captured operation");
        let attempt = start
            .seven_zip_extract_attempt
            .expect("runtime-owned extract attempt context");
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured extraction request");
        };
        let controls = start.controls;
        let cancellation = start.cancellation;
        let inspection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let inspection_worker = inspection_observed.clone();
        let stale_provider_rejection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let stale_provider_rejection_worker = stale_provider_rejection_observed.clone();
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute_with_options(
                token,
                &request,
                Some(attempt),
                Some(provider),
                Some(image),
                cancellation.as_ref(),
                &controls,
                &mut |fact| fact_tx.send(fact).unwrap(),
                WorkerOptions {
                    archive_inspection_observed: Some(inspection_worker),
                    stale_provider_rejection_observed: Some(stale_provider_rejection_worker),
                    attempt_context_rejection_observed: None,
                    destination_parent_rejection_observed: None,
                    tree_cleanup_replay: None,
                    ..WorkerOptions::default()
                },
            );
        });

        let report = match fact_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(ExecutorFact::Terminal {
                token: fact_token,
                report,
            }) => {
                assert_eq!(fact_token, token);
                report
            }
            Ok(ExecutorFact::Progress { .. }) => panic!("stale provider emitted progress"),
            Ok(ExecutorFact::SevenZipExtractBatchIntent { .. }) => {
                panic!("stale provider emitted registration or item intent")
            }
            Ok(fact) => panic!("unexpected pre-terminal worker fact: {fact:?}"),
            Err(error) => panic!("worker did not reject stale provider: {error}"),
        };
        assert_eq!(
            report.outcome,
            crate::operations::model::TerminalOutcome::Failed
        );
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(
            report.receipts[0].outcome,
            crate::operations::model::ItemOutcome::Failed
        );
        assert_eq!(
            report.receipts[0].detail.as_deref(),
            Some("7z extraction was rejected before batch registration")
        );
        assert!(stale_provider_rejection_observed.load(Ordering::Acquire));
        assert!(report.receipts[0].created_archive_path.is_none());
        assert!(report.receipts[0].created_folder_path.is_none());
        assert!(!inspection_observed.load(Ordering::Acquire));

        runtime.commit_fact(ExecutorFact::Terminal { token, report });
        let (predecessors, terminal) = runtime
            .seven_zip_extract_terminal_write_plan(token)
            .expect("frozen terminal write plan");
        assert!(predecessors.is_empty());
        writer
            .upsert_with_reply(terminal.clone())
            .recv_blocking()
            .expect("journal writer replies")
            .expect("terminal snapshot persists");
        assert!(runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal));
        worker.join().unwrap();
        drop(writer);

        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&output).unwrap(), destination_sentinel);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        let persisted_attempt = &persisted["attempts"][0];
        assert!(persisted_attempt["seven_zip_extract_batch"].is_null());
        let terminal_receipts = persisted_attempt["seven_zip_extract_receipts"]
            .as_array()
            .unwrap();
        assert_eq!(terminal_receipts.len(), 1);
        assert_eq!(terminal_receipts[0]["outcome"], "failed");
        assert!(terminal_receipts[0]["output"].is_null());
        assert!(terminal_receipts[0]["publication_identity"].is_null());
        assert!(persisted_attempt["created_archive_path"].is_null());
        assert_eq!(persisted_attempt["terminal_outcome"], "failed");
        assert_eq!(persisted_attempt["receipts_recorded"], true);
        let reopened = crate::operations::journal::JournalStore::open(journal_path).unwrap();
        assert!(reopened.recoveries().is_empty());
        let top_level = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let non_journal_paths = top_level
            .iter()
            .filter(|name| {
                !name
                    .to_string_lossy()
                    .starts_with("stale-provider-journal.json")
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            non_journal_paths, before_namespace,
            "no spool, tree, or output artifacts"
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_rejects_mismatched_attempt_context_before_registration() {
        run_real_worker_mismatched_attempt_context_fixture();
    }

    fn run_real_worker_mismatched_attempt_context_fixture() {
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        let fixture_archive = fixture_root.join("mixed.7z");
        assert!(release.is_file() && seven_zip.is_file() && fixture_archive.is_file());
        let source_bytes = std::fs::read(&fixture_archive).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("mixed.7z");
        std::fs::write(&source, &source_bytes).unwrap();
        let output = temp.path().join("mixed");
        let destination_sentinel = b"foreign destination sentinel";
        std::fs::write(&output, destination_sentinel).unwrap();
        let before_namespace = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();

        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .expect("installed 7-Zip discovery");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .expect("read-only operation binding");
        assert!(provider.commit_discovery(revision, Some(executable)));
        let guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            vec![source.clone()],
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }],
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        let image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease");
        let journal_path = temp.path().join("mismatched-attempt-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime
            .start_next()
            .expect("runtime admits captured operation");
        let authoritative_attempt = start
            .seven_zip_extract_attempt
            .expect("runtime-owned extract attempt context");
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured extraction request");
        };
        let mut mismatched_attempt = authoritative_attempt.clone();
        mismatched_attempt.attempt = mismatched_attempt.attempt.wrapping_add(1).max(1);
        assert_ne!(mismatched_attempt.attempt, authoritative_attempt.attempt);
        let controls = start.controls;
        let cancellation = start.cancellation;
        let inspection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let inspection_worker = inspection_observed.clone();
        let context_rejection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let context_rejection_worker = context_rejection_observed.clone();
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute_with_options(
                token,
                &request,
                Some(mismatched_attempt),
                Some(provider),
                Some(image),
                cancellation.as_ref(),
                &controls,
                &mut |fact| fact_tx.send(fact).unwrap(),
                WorkerOptions {
                    archive_inspection_observed: Some(inspection_worker),
                    attempt_context_rejection_observed: Some(context_rejection_worker),
                    ..WorkerOptions::default()
                },
            );
        });

        let report = match fact_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(ExecutorFact::Terminal {
                token: fact_token,
                report,
            }) => {
                assert_eq!(fact_token, token);
                report
            }
            Ok(ExecutorFact::Progress { .. }) => panic!("mismatched context emitted progress"),
            Ok(ExecutorFact::SevenZipExtractBatchIntent { .. }) => {
                panic!("mismatched context emitted registration or item intent")
            }
            Ok(fact) => panic!("unexpected pre-terminal worker fact: {fact:?}"),
            Err(error) => panic!("worker did not reject mismatched context: {error}"),
        };
        assert_eq!(
            report.outcome,
            crate::operations::model::TerminalOutcome::Failed
        );
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(
            report.receipts[0].outcome,
            crate::operations::model::ItemOutcome::Failed
        );
        assert_eq!(
            report.receipts[0].detail.as_deref(),
            Some("7z extraction was rejected before batch registration")
        );
        assert!(report.receipts[0].created_archive_path.is_none());
        assert!(report.receipts[0].created_folder_path.is_none());
        assert!(context_rejection_observed.load(Ordering::Acquire));
        assert!(!inspection_observed.load(Ordering::Acquire));

        runtime.commit_fact(ExecutorFact::Terminal { token, report });
        let (predecessors, terminal) = runtime
            .seven_zip_extract_terminal_write_plan(token)
            .expect("frozen terminal write plan");
        assert!(predecessors.is_empty());
        writer
            .upsert_with_reply(terminal.clone())
            .recv_blocking()
            .expect("journal writer replies")
            .expect("terminal snapshot persists");
        assert!(runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal));
        worker.join().unwrap();
        drop(writer);

        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&output).unwrap(), destination_sentinel);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        let persisted_attempt = &persisted["attempts"][0];
        assert!(persisted_attempt["seven_zip_extract_batch"].is_null());
        let terminal_receipts = persisted_attempt["seven_zip_extract_receipts"]
            .as_array()
            .unwrap();
        assert_eq!(terminal_receipts.len(), 1);
        assert_eq!(terminal_receipts[0]["outcome"], "failed");
        assert!(terminal_receipts[0]["output"].is_null());
        assert!(terminal_receipts[0]["publication_identity"].is_null());
        assert!(persisted_attempt["created_archive_path"].is_null());
        assert_eq!(persisted_attempt["terminal_outcome"], "failed");
        assert_eq!(persisted_attempt["receipts_recorded"], true);
        let reopened = crate::operations::journal::JournalStore::open(journal_path).unwrap();
        assert!(reopened.recoveries().is_empty());
        let top_level = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let non_journal_paths = top_level
            .iter()
            .filter(|name| {
                !name
                    .to_string_lossy()
                    .starts_with("mismatched-attempt-journal.json")
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            non_journal_paths, before_namespace,
            "no spool, tree, or output artifacts"
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_rejects_replaced_destination_parent_before_source_open() {
        run_real_worker_replaced_destination_parent_fixture();
    }

    fn run_real_worker_replaced_destination_parent_fixture() {
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        let fixture_archive = fixture_root.join("mixed.7z");
        assert!(release.is_file() && seven_zip.is_file() && fixture_archive.is_file());
        let source_bytes = std::fs::read(&fixture_archive).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let original_parent = temp.path().join("parent-original");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("mixed.7z");
        std::fs::write(&source, &source_bytes).unwrap();
        let output = parent.join("mixed");

        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .expect("installed 7-Zip discovery");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .expect("read-only operation binding");
        assert!(provider.commit_discovery(revision, Some(executable)));
        let guard = crate::operations::model::test_zip_create_guard(&source, &output);
        let captured_parent_identity = guard.destination_parent_identity.clone();
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            vec![source.clone()],
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }],
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        let image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease");
        let journal_path = temp.path().join("replaced-parent-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime
            .start_next()
            .expect("runtime admits captured operation");
        let attempt = start
            .seven_zip_extract_attempt
            .expect("runtime-owned extract attempt context");
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured extraction request");
        };

        std::fs::rename(&parent, &original_parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        let sentinel = b"replacement-parent output sentinel";
        std::fs::write(&output, sentinel).unwrap();
        let probe_source = parent.join("identity-probe.7z");
        std::fs::write(&probe_source, b"probe").unwrap();
        let replacement_identity = crate::operations::model::test_zip_create_guard(
            &probe_source,
            &parent.join("unused-output"),
        )
        .destination_parent_identity;
        let replacement_route = super::super::windows::lock_virtual_destination(&parent, None)
            .expect("replacement parent route remains lockable");
        assert_ne!(replacement_identity, captured_parent_identity);
        drop(replacement_route);
        std::fs::remove_file(&probe_source).unwrap();
        assert!(
            !source.exists(),
            "captured source path is absent from replacement parent"
        );
        let replacement_parent_namespace = std::fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            replacement_parent_namespace,
            [std::ffi::OsString::from("mixed")].into_iter().collect()
        );
        let before_namespace = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();

        let controls = start.controls;
        let cancellation = start.cancellation;
        let inspection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let inspection_worker = inspection_observed.clone();
        let parent_rejection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let parent_rejection_worker = parent_rejection_observed.clone();
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute_with_options(
                token,
                &request,
                Some(attempt),
                Some(provider),
                Some(image),
                cancellation.as_ref(),
                &controls,
                &mut |fact| fact_tx.send(fact).unwrap(),
                WorkerOptions {
                    archive_inspection_observed: Some(inspection_worker),
                    destination_parent_rejection_observed: Some(parent_rejection_worker),
                    ..WorkerOptions::default()
                },
            );
        });

        let report = match fact_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(ExecutorFact::Terminal {
                token: fact_token,
                report,
            }) => {
                assert_eq!(fact_token, token);
                report
            }
            Ok(ExecutorFact::Progress { .. }) => panic!("replaced parent emitted progress"),
            Ok(ExecutorFact::SevenZipExtractBatchIntent { .. }) => {
                panic!("replaced parent emitted registration or item intent")
            }
            Ok(fact) => panic!("unexpected pre-terminal worker fact: {fact:?}"),
            Err(error) => panic!("worker did not reject replaced parent: {error}"),
        };
        assert_eq!(
            report.outcome,
            crate::operations::model::TerminalOutcome::Failed
        );
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(
            report.receipts[0].outcome,
            crate::operations::model::ItemOutcome::Failed
        );
        assert_eq!(
            report.receipts[0].detail.as_deref(),
            Some("7z extraction was rejected before batch registration")
        );
        assert!(report.receipts[0].created_archive_path.is_none());
        assert!(report.receipts[0].created_folder_path.is_none());
        assert!(parent_rejection_observed.load(Ordering::Acquire));
        assert!(!inspection_observed.load(Ordering::Acquire));

        runtime.commit_fact(ExecutorFact::Terminal { token, report });
        let (predecessors, terminal) = runtime
            .seven_zip_extract_terminal_write_plan(token)
            .expect("frozen terminal write plan");
        assert!(predecessors.is_empty());
        writer
            .upsert_with_reply(terminal.clone())
            .recv_blocking()
            .expect("journal writer replies")
            .expect("terminal snapshot persists");
        assert!(runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal));
        worker.join().unwrap();
        drop(writer);

        assert_eq!(
            std::fs::read(original_parent.join("mixed.7z")).unwrap(),
            source_bytes
        );
        assert_eq!(std::fs::read(&output).unwrap(), sentinel);
        assert!(!source.exists());
        assert_eq!(
            std::fs::read_dir(&parent)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>(),
            replacement_parent_namespace
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        let persisted_attempt = &persisted["attempts"][0];
        assert!(persisted_attempt["seven_zip_extract_batch"].is_null());
        let terminal_receipts = persisted_attempt["seven_zip_extract_receipts"]
            .as_array()
            .unwrap();
        assert_eq!(terminal_receipts.len(), 1);
        assert_eq!(terminal_receipts[0]["outcome"], "failed");
        assert!(terminal_receipts[0]["output"].is_null());
        assert!(terminal_receipts[0]["publication_identity"].is_null());
        assert_eq!(persisted_attempt["terminal_outcome"], "failed");
        assert_eq!(persisted_attempt["receipts_recorded"], true);
        let reopened = crate::operations::journal::JournalStore::open(journal_path).unwrap();
        assert!(reopened.recoveries().is_empty());
        let top_level = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let non_journal_paths = top_level
            .iter()
            .filter(|name| {
                !name
                    .to_string_lossy()
                    .starts_with("replaced-parent-journal.json")
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            non_journal_paths, before_namespace,
            "no spool or tree artifacts"
        );
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_ordered_multi_source_keeps_prefix_and_continues_after_collision_and_malformed() {
        run_real_worker_ordered_scenario(false, None);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_cancel_between_published_items_preserves_prefix_and_skips_later_source() {
        run_real_worker_ordered_scenario(true, None);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_unresolved_middle_item_stops_later_intents_and_keeps_prefix() {
        run_real_worker_ordered_scenario(false, Some(1));
    }

    fn run_real_worker_ordered_scenario(
        cancel_after_first_published: bool,
        fail_after_spool_owned_ordinal: Option<usize>,
    ) {
        let unresolved = fail_after_spool_owned_ordinal.is_some();
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        assert!(release.is_file() && seven_zip.is_file());
        let temp = tempfile::tempdir().unwrap();
        let mut sources = Vec::new();
        let mut fixture_labels = Vec::new();
        if cancel_after_first_published {
            fixture_labels.extend([("first.7z", "mixed"), ("last.7z", "implicit")]);
        } else if unresolved {
            fixture_labels.extend([
                ("first.7z", "mixed"),
                ("blocked.7z", "implicit"),
                ("last.7z", "implicit"),
            ]);
        } else {
            fixture_labels.extend([
                ("first.7z", "mixed"),
                ("collision.7z", "implicit"),
                ("malformed.7z", "malformed"),
                ("last.7z", "implicit"),
            ]);
        }
        for (source_name, fixture_name) in &fixture_labels {
            let source = temp.path().join(source_name);
            if *fixture_name == "malformed" {
                std::fs::write(&source, b"not a 7z archive").unwrap();
            } else {
                std::fs::copy(fixture_root.join(format!("{fixture_name}.7z")), &source).unwrap();
            }
            sources.push(source);
        }
        let collision = temp.path().join("collision");
        if !cancel_after_first_published && !unresolved {
            std::fs::create_dir(&collision).unwrap();
            std::fs::write(collision.join("foreign-sentinel"), b"preserve me").unwrap();
        }

        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .expect("installed 7-Zip discovery");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .expect("read-only operation binding");
        assert!(provider.commit_discovery(revision, Some(executable)));
        let source_guards = sources
            .iter()
            .map(|source| {
                let guard = crate::operations::model::test_zip_create_guard(
                    source,
                    &temp.path().join("unused-output"),
                );
                crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }
            })
            .collect::<Vec<_>>();
        let parent_guard = crate::operations::model::test_zip_create_guard(
            &sources[0],
            &temp.path().join("unused-output"),
        );
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            sources.clone(),
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: parent_guard.destination_parent_identity,
                sources: source_guards,
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        let image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease");
        let journal_path = temp.path().join("ordered-worker-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let attempt = start
            .seven_zip_extract_attempt
            .expect("runtime-owned extract attempt context");
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured extraction request");
        };
        let controls = start.controls;
        let cancellation = start.cancellation;
        let expected_attempt = attempt.clone();
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute_with_options(
                token,
                &request,
                Some(attempt),
                Some(provider),
                Some(image),
                cancellation.as_ref(),
                &controls,
                &mut |fact| fact_tx.send(fact).unwrap(),
                WorkerOptions {
                    fail_after_spool_owned: false,
                    fail_after_spool_owned_ordinal,
                    fail_spool_working_clone_after_create: false,
                    fail_spool_identity_after_create: false,
                    fail_tree_identity_after_create: false,
                    cancel_during_materialize_after_child: false,
                    forget_materializer_child_ids: false,
                    expire_publish_deadline_after_intent: false,
                    cancel_after_spool_intent_ack: false,
                    expire_after_spool_intent_ack: false,
                    expire_after_tree_intent_ack: false,
                    expire_after_decode: false,
                    expire_during_materialize_after_child: false,
                    close_uncertain_tree_cleanup: false,
                    expire_tree_cleanup_after_identity: false,
                    cancel_after_publish_intent_ack: false,
                    fail_publish_verification_after_rename: false,
                    decode_drain_fixture: None,
                    decode_drain_marker: None,
                    decode_drain_completed: None,
                    archive_inspection_observed: None,
                    stale_provider_rejection_observed: None,
                    attempt_context_rejection_observed: None,
                    destination_parent_rejection_observed: None,
                    tree_cleanup_replay: None,
                },
            );
            cancellation.as_ref().load(Ordering::Acquire)
        });
        let mut first_published = false;
        let mut unresolved_spool = None;
        let report = loop {
            match fact_rx.recv_timeout(Duration::from_secs(120)) {
                Ok(ExecutorFact::SevenZipExtractBatchIntent { token, batch }) => {
                    assert_eq!(batch.envelope.attempt, expected_attempt);
                    assert_eq!(batch.envelope.items.len(), sources.len());
                    assert!(batch.validate().is_ok());
                    if unresolved {
                        assert!(batch.items.iter().all(|ledger| ledger.binding.item < 2));
                        if batch
                            .items
                            .last()
                            .is_some_and(|ledger| ledger.binding.item == 1)
                        {
                            unresolved_spool = Some(
                                super::path_from_journal(&batch.envelope.items[1].spool)
                                    .expect("registered blocked spool path"),
                            );
                        }
                    } else if !cancel_after_first_published {
                        assert!(
                            batch
                                .items
                                .iter()
                                .all(|ledger| { matches!(ledger.binding.item, 0 | 3) }),
                            "collision and malformed sources never acquire item intents"
                        );
                    }
                    runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
                        token,
                        batch: batch.clone(),
                    });
                    let record = runtime
                        .claim_pending_seven_zip_extract_batch_write(token, &batch)
                        .unwrap_or_else(|| panic!("Runtime rejected batch: {batch:#?}"));
                    writer
                        .upsert_with_reply(record)
                        .recv_blocking()
                        .expect("journal writer replies")
                        .expect("batch snapshot persists");
                    assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                    if cancel_after_first_published
                        && batch.items.first().is_some_and(|ledger| {
                            ledger.binding.item == 0 && ledger.phase == Phase::Published
                        })
                    {
                        assert!(!first_published);
                        first_published = true;
                        assert!(runtime.cancel(token).is_none());
                    }
                }
                Ok(ExecutorFact::Terminal { token, report }) => {
                    if cancel_after_first_published {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Cancelled
                        );
                        assert_eq!(report.receipts.len(), 2);
                        assert_eq!(
                            report.receipts[0].outcome,
                            crate::operations::model::ItemOutcome::Completed
                        );
                        assert_eq!(
                            report.receipts[1].outcome,
                            crate::operations::model::ItemOutcome::CancelledBeforeStart
                        );
                    } else if unresolved {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        assert_eq!(report.receipts.len(), 3);
                        assert_eq!(
                            report.receipts[0].outcome,
                            crate::operations::model::ItemOutcome::Completed
                        );
                        assert_eq!(
                            report.receipts[1].outcome,
                            crate::operations::model::ItemOutcome::Unknown
                        );
                        assert_eq!(
                            report.receipts[2].outcome,
                            crate::operations::model::ItemOutcome::Unknown
                        );
                        assert!(unresolved_spool.as_ref().is_some_and(|path| path.exists()));
                    } else {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Partial
                        );
                        assert_eq!(report.receipts.len(), 4);
                        assert_eq!(
                            report.receipts[0].outcome,
                            crate::operations::model::ItemOutcome::Completed
                        );
                        assert_eq!(
                            report.receipts[1].outcome,
                            crate::operations::model::ItemOutcome::Failed
                        );
                        assert_eq!(
                            report.receipts[2].outcome,
                            crate::operations::model::ItemOutcome::Failed
                        );
                        assert_eq!(
                            report.receipts[3].outcome,
                            crate::operations::model::ItemOutcome::Completed
                        );
                    }
                    runtime.commit_fact(ExecutorFact::Terminal {
                        token,
                        report: report.clone(),
                    });
                    let (predecessors, terminal) = runtime
                        .seven_zip_extract_terminal_write_plan(token)
                        .expect("frozen terminal write plan");
                    assert!(predecessors.is_empty());
                    writer
                        .upsert_with_reply(terminal.clone())
                        .recv_blocking()
                        .expect("journal writer replies")
                        .expect("terminal snapshot persists");
                    assert!(
                        runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal)
                    );
                    break report;
                }
                Ok(fact) => {
                    runtime.commit_fact(fact);
                }
                Err(error) => panic!("worker did not produce its terminal fact: {error}"),
            }
        };
        assert_eq!(worker.join().unwrap(), cancel_after_first_published);
        drop(writer);
        let first_output = temp.path().join("first");
        let last_output = temp.path().join("last");
        assert!(first_output.is_dir());
        assert_eq!(
            tree_snapshot(&first_output),
            tree_snapshot(&fixture_root.join("mixed-source"))
        );
        if cancel_after_first_published {
            assert!(
                !last_output.exists(),
                "later source was not inspected or mutated"
            );
            assert!(first_published);
        } else if unresolved {
            assert!(!last_output.exists(), "later source has no output");
            assert!(!temp.path().join("blocked").exists());
            let spool = unresolved_spool
                .as_ref()
                .expect("unknown spool path remains journaled");
            assert!(spool.is_file(), "unresolved spool remains for recovery");
        } else {
            assert_eq!(
                tree_snapshot(&last_output),
                tree_snapshot(&fixture_root.join("implicit-source"))
            );
            assert_eq!(
                std::fs::read(collision.join("foreign-sentinel")).unwrap(),
                b"preserve me"
            );
            assert!(!temp.path().join("malformed").exists());
            assert!(
                report.receipts[1]
                    .detail
                    .as_deref()
                    .is_some_and(|s| s.contains("output"))
            );
            assert!(
                report.receipts[2]
                    .detail
                    .as_deref()
                    .is_some_and(|s| s.contains("archive"))
            );
        }
        let mut actual_top_level = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        actual_top_level.sort();
        let mut expected_top_level = if cancel_after_first_published {
            vec![
                "first".into(),
                "first.7z".into(),
                "last.7z".into(),
                "ordered-worker-journal.json".into(),
                "ordered-worker-journal.json.bak".into(),
            ]
        } else if unresolved {
            vec![
                "first".into(),
                "first.7z".into(),
                "blocked.7z".into(),
                "last.7z".into(),
                unresolved_spool
                    .as_ref()
                    .expect("spool path retained")
                    .file_name()
                    .unwrap()
                    .to_os_string(),
                "ordered-worker-journal.json".into(),
                "ordered-worker-journal.json.bak".into(),
            ]
        } else {
            vec![
                "first".into(),
                "first.7z".into(),
                "collision".into(),
                "collision.7z".into(),
                "last".into(),
                "last.7z".into(),
                "malformed.7z".into(),
                "ordered-worker-journal.json".into(),
                "ordered-worker-journal.json.bak".into(),
            ]
        };
        expected_top_level.sort();
        assert_eq!(
            actual_top_level, expected_top_level,
            "no temporary artifacts remain"
        );
        let reopened =
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap();
        assert_eq!(reopened.attempt_keys().len(), 1);
        assert!(matches!(
            reopened.recoveries().as_slice(),
            [crate::operations::journal::RecoveryItem::RecoveryRequired(record)]
                if record.state == crate::operations::journal::JournalState::RecoveryRequired
        ));
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(journal_path).unwrap()).unwrap();
        assert_eq!(
            persisted["attempts"][0]["seven_zip_extract_receipts"]
                .as_array()
                .unwrap()
                .len(),
            report.receipts.len()
        );
    }

    fn run_real_worker_success_fixture(
        fixture_name: &str,
        fail_published_write: bool,
        fail_materialize_cleanup: bool,
        fail_publish_deadline: bool,
        collide_spool: bool,
        collide_tree: bool,
        fail_tree_identity: bool,
        fail_tree_deadline: bool,
        fail_decode_deadline: bool,
        fail_copy_deadline: bool,
        fail_close_uncertain: bool,
        fail_publish_cancel: bool,
        collide_publish_after_intent: bool,
        fail_publish_verification: bool,
        expire_cleanup_after_root_identity: bool,
    ) {
        run_real_worker_fixture_with_drain(
            fixture_name,
            fail_published_write,
            fail_materialize_cleanup,
            fail_publish_deadline,
            collide_spool,
            collide_tree,
            fail_tree_identity,
            fail_tree_deadline,
            fail_decode_deadline,
            fail_copy_deadline,
            fail_close_uncertain,
            fail_publish_cancel,
            collide_publish_after_intent,
            fail_publish_verification,
            expire_cleanup_after_root_identity,
            None,
            false,
            false,
        );
    }

    fn run_real_worker_fixture_with_drain(
        fixture_name: &str,
        fail_published_write: bool,
        fail_materialize_cleanup: bool,
        fail_publish_deadline: bool,
        collide_spool: bool,
        collide_tree: bool,
        fail_tree_identity: bool,
        fail_tree_deadline: bool,
        fail_decode_deadline: bool,
        fail_copy_deadline: bool,
        fail_close_uncertain: bool,
        fail_publish_cancel: bool,
        collide_publish_after_intent: bool,
        fail_publish_verification: bool,
        expire_cleanup_after_root_identity: bool,
        drain_fixture: Option<DecodeDrainFixture>,
        replace_source_during_registration_ack: bool,
        replay_tree_cleanup: bool,
    ) {
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        let fixture_archive = fixture_root.join(format!("{fixture_name}.7z"));
        let fixture_expected = fixture_root.join(format!("{fixture_name}-source"));
        assert!(release.is_file() && seven_zip.is_file() && fixture_archive.is_file());
        let source_bytes = std::fs::read(&fixture_archive).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join(format!("{fixture_name}.7z"));
        std::fs::write(&source, &source_bytes).unwrap();
        let drain_marker = temp.path().join("helper-descendant.pid");
        let drain_marker_worker = drain_marker.clone();
        let drain_completed = std::sync::Arc::new(AtomicBool::new(false));
        let drain_completed_worker = drain_completed.clone();
        let archive_inspection_observed = std::sync::Arc::new(AtomicBool::new(false));
        let archive_inspection_worker =
            replace_source_during_registration_ack.then(|| archive_inspection_observed.clone());

        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .expect("installed 7-Zip discovery");
        assert_eq!(executable.path, seven_zip);
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .expect("read-only operation binding");
        assert!(provider.commit_discovery(revision, Some(executable)));

        let guard = crate::operations::model::test_zip_create_guard(
            &source,
            &temp.path().join("unused-output"),
        );
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            vec![source.clone()],
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }],
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        let image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .expect("explicit release image lease");

        let journal_path = temp.path().join("worker-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let attempt = start
            .seven_zip_extract_attempt
            .expect("runtime-owned extract attempt context");
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured extraction request");
        };
        let original_controls = start.controls;
        let (controls, control_observations, control_bridge, replay_sender) =
            if fail_published_write || replay_tree_cleanup {
                let (worker_control_tx, worker_control_rx) = std::sync::mpsc::channel();
                let replay_sender = replay_tree_cleanup.then(|| worker_control_tx.clone());
                let (observed_control_tx, observed_control_rx) = std::sync::mpsc::channel();
                let bridge = std::thread::spawn(move || {
                    let mut failed_published = None;
                    let mut late_observations = 0usize;
                    while let Ok(control) = original_controls.recv() {
                        match &control {
                            WorkerControl::SevenZipExtractBatchJournalFailedExact {
                                batch, ..
                            } if batch
                                .items
                                .first()
                                .is_some_and(|item| item.phase == Phase::Published) =>
                            {
                                assert!(failed_published.replace(batch.clone()).is_none());
                            }
                            WorkerControl::SevenZipExtractBatchObservedExact { batch, .. }
                                if failed_published.as_ref() == Some(batch) =>
                            {
                                late_observations += 1;
                                assert!(
                                    late_observations <= 2,
                                    "only the two late ACKs observe the failed batch"
                                );
                            }
                            WorkerControl::SevenZipExtractBatchJournaledExact { batch, .. }
                                if failed_published.as_ref() == Some(batch) =>
                            {
                                panic!("late exact observation must not grant forward permission");
                            }
                            WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                                batch,
                                ..
                            } if failed_published.as_ref() == Some(batch) => {
                                panic!("late exact observation must not grant cleanup permission");
                            }
                            WorkerControl::SevenZipExtractTerminalJournaledExact { .. } => {
                                if fail_published_write {
                                    assert_eq!(
                                        late_observations, 2,
                                        "both late ACK controls precede terminal ACK"
                                    );
                                }
                            }
                            _ => {}
                        }
                        let terminal = matches!(
                            control,
                            WorkerControl::SevenZipExtractTerminalJournaledExact { .. }
                        );
                        observed_control_tx
                            .send(control.clone())
                            .expect("test observes each Runtime control");
                        worker_control_tx
                            .send(control)
                            .expect("control reaches the real worker in FIFO order");
                        if terminal {
                            break;
                        }
                    }
                });
                (
                    worker_control_rx,
                    Some(observed_control_rx),
                    Some(bridge),
                    replay_sender,
                )
            } else {
                (original_controls, None, None, None)
            };
        let cancellation = start.cancellation;
        let cancellation_worker = cancellation.clone();
        let expected_request = request.clone();
        let expected_attempt = attempt.clone();
        let mut injected_published_failure = false;
        let mut replayed_tree_cleanup_control = None;
        let mut tree_delete_intents = 0usize;
        let mut worker_progress_facts = 0usize;
        let mut saw_spool_deleted = false;
        let mut saw_tree_deleted = false;
        let mut saw_discarded = false;
        let mut drain_cancel_sent = false;
        let mut drain_descendant_seen = false;
        let mut expired_stage_path = None;
        let mut foreign_collision_path = None;
        let mut worker_spool_path = None;
        let mut worker_tree_path = None;
        let mut uncertain_tree_path = None;
        let mut replacement_bytes = None;
        let mut saw_item_intent = false;
        let foreign_collision_bytes = b"foreign exact sentinel";
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let (tree_replay_seam, unmatched_tree_control_rx) = if replay_tree_cleanup {
            let (unmatched_tx, unmatched_rx) = std::sync::mpsc::channel();
            (
                Some(TreeCleanupReplaySeam {
                    native_cleanup_finished: std::sync::Arc::new(std::sync::Barrier::new(2)),
                    release_successor: std::sync::Arc::new(std::sync::Barrier::new(2)),
                    unmatched_control: unmatched_tx,
                }),
                Some(unmatched_rx),
            )
        } else {
            (None, None)
        };
        let tree_replay_seam_worker = tree_replay_seam.clone();
        let worker = std::thread::spawn(move || {
            super::execute_with_options(
                token,
                &request,
                Some(attempt),
                Some(provider),
                Some(image),
                cancellation_worker.as_ref(),
                &controls,
                &mut |fact| fact_tx.send(fact).unwrap(),
                WorkerOptions {
                    fail_after_spool_owned: false,
                    fail_after_spool_owned_ordinal: None,
                    fail_spool_working_clone_after_create: false,
                    fail_spool_identity_after_create: false,
                    fail_tree_identity_after_create: fail_tree_identity,
                    cancel_during_materialize_after_child: fail_materialize_cleanup
                        || expire_cleanup_after_root_identity,
                    forget_materializer_child_ids: fail_materialize_cleanup,
                    expire_publish_deadline_after_intent: fail_publish_deadline,
                    cancel_after_spool_intent_ack: false,
                    expire_after_spool_intent_ack: false,
                    expire_after_tree_intent_ack: fail_tree_deadline,
                    expire_after_decode: fail_decode_deadline,
                    expire_during_materialize_after_child: fail_copy_deadline
                        || fail_close_uncertain,
                    close_uncertain_tree_cleanup: fail_close_uncertain,
                    expire_tree_cleanup_after_identity: expire_cleanup_after_root_identity,
                    cancel_after_publish_intent_ack: fail_publish_cancel,
                    fail_publish_verification_after_rename: fail_publish_verification,
                    decode_drain_fixture: drain_fixture,
                    decode_drain_marker: drain_fixture.map(|_| drain_marker_worker),
                    decode_drain_completed: drain_fixture.map(|_| drain_completed_worker),
                    archive_inspection_observed: archive_inspection_worker,
                    stale_provider_rejection_observed: None,
                    attempt_context_rejection_observed: None,
                    destination_parent_rejection_observed: None,
                    tree_cleanup_replay: tree_replay_seam_worker,
                },
            );
        });

        loop {
            match fact_rx.recv_timeout(if drain_fixture.is_some() {
                Duration::from_millis(25)
            } else {
                Duration::from_secs(120)
            }) {
                Ok(ExecutorFact::SevenZipExtractBatchIntent { token, batch }) => {
                    saw_item_intent |= batch.sequence != 1 || !batch.items.is_empty();
                    assert_eq!(batch.envelope.attempt, expected_attempt);
                    assert_eq!(batch.envelope.provider_revision, revision);
                    let guard = expected_request.seven_zip_extract_guard.as_ref().unwrap();
                    let (parent_volume, parent_index) =
                        guard.destination_parent_identity.windows_parts().unwrap();
                    assert_eq!(batch.envelope.destination_parent.volume, parent_volume);
                    assert_eq!(batch.envelope.destination_parent.file_index, parent_index);
                    assert_eq!(batch.envelope.items.len(), expected_request.sources.len());
                    assert_eq!(
                        batch.envelope.items[0].source_path,
                        JournalPath::from_path(&expected_request.sources[0])
                    );
                    assert_eq!(
                        batch.envelope.items[0].output,
                        JournalPath::from_path(&temp.path().join(fixture_name))
                    );
                    assert!(batch.validate().is_ok());
                    worker_spool_path = Some(
                        super::path_from_journal(&batch.envelope.items[0].spool)
                            .expect("registered spool path"),
                    );
                    worker_tree_path = Some(
                        super::path_from_journal(&batch.envelope.items[0].tree)
                            .expect("registered tree path"),
                    );
                    if drain_fixture.is_some()
                        && batch
                            .items
                            .first()
                            .is_some_and(|ledger| ledger.spool_state == ObjectState::DeleteIntent)
                    {
                        assert!(drain_descendant_seen);
                        assert!(drain_completed.load(Ordering::Acquire));
                        let pid: u32 = std::fs::read_to_string(&drain_marker)
                            .unwrap()
                            .parse()
                            .unwrap();
                        crate::archive_provider::worker_fixture_descendant_stopped_for_test(pid);
                    }
                    if batch
                        .items
                        .first()
                        .is_some_and(|ledger| ledger.tree_state == ObjectState::DeleteIntent)
                    {
                        tree_delete_intents += 1;
                        if fail_materialize_cleanup
                            || fail_copy_deadline
                            || expire_cleanup_after_root_identity
                        {
                            let tree = super::path_from_journal(&batch.envelope.items[0].tree)
                                .expect("registered staging tree path");
                            let partial = tree_snapshot(&tree);
                            let node_count = partial.len();
                            let written_bytes = partial
                                .values()
                                .filter_map(Option::as_ref)
                                .map(Vec::len)
                                .sum::<usize>();
                            assert!(node_count >= 2, "child handles remain held");
                            assert!(written_bytes > 0, "confirmed prefix bytes remain in stage");
                            assert!(std::fs::rename(&tree, tree.with_extension("held")).is_err());
                            let written = partial
                                .iter()
                                .find_map(|(path, bytes)| {
                                    bytes
                                        .as_ref()
                                        .filter(|bytes| !bytes.is_empty())
                                        .map(|_| path)
                                })
                                .expect("the real native copy wrote a file before cancellation");
                            assert!(
                                std::fs::OpenOptions::new()
                                    .write(true)
                                    .open(written)
                                    .is_err(),
                                "the staged child stays pinned while cleanup intent is pending"
                            );
                        }
                        if fail_close_uncertain {
                            let tree = super::path_from_journal(&batch.envelope.items[0].tree)
                                .expect("registered staging tree path");
                            assert!(tree.is_dir());
                            assert!(std::fs::rename(&tree, tree.with_extension("held")).is_err());
                        }
                    }
                    if let Some(ledger) = batch.items.first() {
                        saw_spool_deleted |= ledger.spool_state == ObjectState::Deleted;
                        saw_tree_deleted |= ledger.tree_state == ObjectState::Deleted;
                        saw_discarded |= ledger.phase == Phase::Discarded;
                    }
                    if (fail_publish_deadline
                        || fail_publish_cancel
                        || collide_publish_after_intent
                        || fail_publish_verification)
                        && batch
                            .items
                            .first()
                            .is_some_and(|ledger| ledger.phase == Phase::PublishIntent)
                    {
                        expired_stage_path =
                            Some(super::path_from_journal(&batch.envelope.items[0].tree).unwrap());
                    }
                    let phase = batch.items.first().map(|ledger| ledger.phase);
                    if collide_publish_after_intent
                        && phase == Some(Phase::PublishIntent)
                        && foreign_collision_path.is_none()
                    {
                        let output = super::path_from_journal(&batch.envelope.items[0].output)
                            .expect("registered output path");
                        assert!(!output.exists());
                        std::fs::create_dir(&output).unwrap();
                        let sentinel = output.join("foreign-sentinel.bin");
                        std::fs::write(&sentinel, foreign_collision_bytes).unwrap();
                        foreign_collision_path = Some(sentinel);
                    }
                    if collide_spool
                        && phase == Some(Phase::SpoolCreateIntent)
                        && foreign_collision_path.is_none()
                    {
                        let path =
                            super::path_from_journal(&batch.envelope.items[0].spool).unwrap();
                        assert!(!path.exists());
                        std::fs::write(&path, foreign_collision_bytes).unwrap();
                        foreign_collision_path = Some(path);
                    }
                    if collide_tree
                        && phase == Some(Phase::TreeCreateIntent)
                        && foreign_collision_path.is_none()
                    {
                        let path = super::path_from_journal(&batch.envelope.items[0].tree).unwrap();
                        assert!(!path.exists());
                        std::fs::write(&path, foreign_collision_bytes).unwrap();
                        foreign_collision_path = Some(path);
                    }
                    if (fail_tree_identity
                        || fail_tree_deadline
                        || fail_decode_deadline
                        || expire_cleanup_after_root_identity
                        || fail_close_uncertain)
                        && uncertain_tree_path.is_none()
                    {
                        uncertain_tree_path =
                            Some(super::path_from_journal(&batch.envelope.items[0].tree).unwrap());
                    }
                    assert!(
                        !injected_published_failure,
                        "no later batch intent may follow failed Published persistence"
                    );
                    runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
                        token,
                        batch: batch.clone(),
                    });
                    let record = runtime
                        .claim_pending_seven_zip_extract_batch_write(token, &batch)
                        .unwrap_or_else(|| {
                            panic!("Runtime rejected registration batch: {batch:#?}")
                        });
                    if fail_published_write
                        && !injected_published_failure
                        && batch
                            .items
                            .first()
                            .is_some_and(|ledger| ledger.phase == Phase::Published)
                    {
                        // Simulate the real writer's failed Published persistence response.
                        assert!(runtime.reject_seven_zip_extract_batch(token, &batch));
                        injected_published_failure = true;
                        let expected_batch = serde_json::to_value(&batch)
                            .expect("Published batch serializes for readback");
                        writer
                            .upsert_with_reply(record)
                            .recv_blocking()
                            .expect("journal writer replies")
                            .expect("late exact Published snapshot persists");
                        let persisted: serde_json::Value = serde_json::from_slice(
                            &std::fs::read(&journal_path).expect("late snapshot is readable"),
                        )
                        .expect("late snapshot decodes");
                        let mut values = vec![&persisted];
                        let mut found_exact_batch = false;
                        while let Some(value) = values.pop() {
                            if value.get("seven_zip_extract_batch") == Some(&expected_batch) {
                                found_exact_batch = true;
                                break;
                            }
                            match value {
                                serde_json::Value::Array(items) => values.extend(items.iter()),
                                serde_json::Value::Object(fields) => values.extend(fields.values()),
                                _ => {}
                            }
                        }
                        assert!(
                            found_exact_batch,
                            "the exact failed Published batch is readable"
                        );
                        let observation_rx = control_observations
                            .as_ref()
                            .expect("failure fixture observes Runtime controls");
                        loop {
                            let observed = observation_rx
                                .recv()
                                .expect("failed-write control bridge remains alive");
                            if observed
                                == (WorkerControl::SevenZipExtractBatchJournalFailedExact {
                                    token,
                                    batch: batch.clone(),
                                })
                            {
                                break;
                            }
                        }
                        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                        assert_eq!(
                            observation_rx.recv().expect("first late ACK is observed"),
                            WorkerControl::SevenZipExtractBatchObservedExact {
                                token,
                                batch: batch.clone(),
                            }
                        );
                        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                        assert_eq!(
                            observation_rx
                                .recv()
                                .expect("replayed late ACK is observed"),
                            WorkerControl::SevenZipExtractBatchObservedExact {
                                token,
                                batch: batch.clone(),
                            }
                        );
                        let (failed_predecessor, _, pending_observation) = runtime
                            .seven_zip_extract_repair_records(token)
                            .expect("failed Published snapshot remains repairable");
                        assert_eq!(failed_predecessor, batch);
                        assert!(pending_observation.is_none());
                        continue;
                    }
                    if fail_publish_deadline
                        && batch
                            .items
                            .first()
                            .is_some_and(|ledger| ledger.phase == Phase::PublishIntent)
                    {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                    writer
                        .upsert_with_reply(record)
                        .recv_blocking()
                        .expect("journal writer replies")
                        .expect("batch snapshot persists");
                    if replace_source_during_registration_ack
                        && batch.sequence == 1
                        && batch.items.is_empty()
                    {
                        let guard = expected_request.seven_zip_extract_guard.as_ref().unwrap();
                        let (volume, file_index) =
                            guard.sources[0].source_identity.windows_parts().unwrap();
                        assert_eq!(batch.envelope.items[0].source_identity.volume, volume);
                        assert_eq!(
                            batch.envelope.items[0].source_identity.file_index,
                            file_index
                        );
                        let displaced = source.with_extension("captured-original.7z");
                        std::fs::rename(&source, displaced).expect("move captured source aside");
                        let sentinel = b"replacement sentinel: not the captured archive";
                        std::fs::write(&source, sentinel)
                            .expect("install replacement ordinary file");
                        replacement_bytes = Some(sentinel.as_slice());
                    }
                    if replay_tree_cleanup {
                        if batch.items.is_empty() {
                            assert_eq!(batch.sequence, 1, "only registration has no ledger rows");
                            assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                            assert_eq!(
                                control_observations
                                    .as_ref()
                                    .expect("replay fixture observes Runtime controls")
                                    .recv_timeout(Duration::from_secs(30))
                                    .expect("registration ACK reaches worker bridge"),
                                WorkerControl::SevenZipExtractBatchJournaledExact {
                                    token,
                                    batch: batch.clone(),
                                }
                            );
                            continue;
                        }
                        let ledger = batch.items.first().expect("single fixture item ledger");
                        let observation_rx = control_observations
                            .as_ref()
                            .expect("replay fixture observes Runtime-issued controls");
                        let withhold_ack = (ledger.tree_state == ObjectState::Deleted
                            && ledger.spool_state == ObjectState::Owned)
                            || ledger.spool_state == ObjectState::DeleteIntent;
                        let accepted_control = if withhold_ack {
                            None
                        } else {
                            let cancelled_at_ack = cancellation.load(Ordering::Acquire);
                            assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                            let observed = observation_rx
                                .recv_timeout(Duration::from_secs(30))
                                .expect("Runtime control reaches the fixture bridge");
                            if ledger.tree_state == ObjectState::DeleteIntent {
                                assert_eq!(
                                    observed,
                                    WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                                        token,
                                        batch: batch.clone(),
                                        item: ItemId(ledger.binding.item),
                                        target: crate::operations::runtime::SevenZipExtractCleanupTarget::Tree,
                                    }
                                );
                            } else if ledger.spool_state == ObjectState::DeleteIntent {
                                assert_eq!(
                                    observed,
                                    WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                                        token,
                                        batch: batch.clone(),
                                        item: ItemId(ledger.binding.item),
                                        target: crate::operations::runtime::SevenZipExtractCleanupTarget::Spool,
                                    }
                                );
                            } else {
                                let expected = if cancelled_at_ack {
                                    WorkerControl::SevenZipExtractBatchObservedExact {
                                        token,
                                        batch: batch.clone(),
                                    }
                                } else {
                                    WorkerControl::SevenZipExtractBatchJournaledExact {
                                        token,
                                        batch: batch.clone(),
                                    }
                                };
                                assert_eq!(observed, expected);
                            }
                            Some(observed)
                        };
                        if ledger.tree_state == ObjectState::DeleteIntent {
                            let accepted = accepted_control.expect("Tree cleanup ACK was observed");
                            assert!(
                                replayed_tree_cleanup_control
                                    .replace(accepted.clone())
                                    .is_none()
                            );
                            tree_replay_seam
                                .as_ref()
                                .expect("test worker rendezvous exists")
                                .native_cleanup_finished
                                .wait();
                            assert!(!worker_tree_path.as_ref().unwrap().exists());
                            assert!(worker_spool_path.as_ref().unwrap().is_file());
                            replay_sender
                                .as_ref()
                                .expect("test control bridge sender exists")
                                .send(accepted)
                                .expect("replay exact Runtime control through worker bridge");
                            tree_replay_seam.as_ref().unwrap().release_successor.wait();
                        } else if ledger.tree_state == ObjectState::Deleted
                            && ledger.spool_state == ObjectState::Owned
                        {
                            let unmatched = unmatched_tree_control_rx
                                .as_ref()
                                .expect("worker reports unmatched replay in this mode")
                                .recv_timeout(Duration::from_secs(30))
                                .expect("worker rejects stale Tree ACK before successor ACK");
                            assert_eq!(
                                unmatched,
                                replayed_tree_cleanup_control
                                    .take()
                                    .expect("exact Tree cleanup ACK was retained for replay")
                            );
                            assert!(!worker_tree_path.as_ref().unwrap().exists());
                            assert!(worker_spool_path.as_ref().unwrap().is_file());
                            assert!(!temp.path().join(fixture_name).exists());
                            assert!(
                                matches!(
                                    fact_rx.try_recv(),
                                    Err(std::sync::mpsc::TryRecvError::Empty)
                                ),
                                "no successor batch or terminal arrives while its ACK is withheld"
                            );
                            let cancelled_at_ack = cancellation.load(Ordering::Acquire);
                            assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                            assert_eq!(
                                observation_rx
                                    .recv_timeout(Duration::from_secs(30))
                                    .expect("TreeDeleted normal ACK reaches worker bridge"),
                                if cancelled_at_ack {
                                    WorkerControl::SevenZipExtractBatchObservedExact {
                                        token,
                                        batch: batch.clone(),
                                    }
                                } else {
                                    WorkerControl::SevenZipExtractBatchJournaledExact {
                                        token,
                                        batch: batch.clone(),
                                    }
                                }
                            );
                            continue;
                        } else if ledger.spool_state == ObjectState::DeleteIntent {
                            assert!(!worker_tree_path.as_ref().unwrap().exists());
                            assert!(worker_spool_path.as_ref().unwrap().is_file());
                            assert!(
                                matches!(
                                    fact_rx.try_recv(),
                                    Err(std::sync::mpsc::TryRecvError::Empty)
                                ),
                                "spool remains pinned while its exact cleanup ACK is withheld"
                            );
                            assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                            assert_eq!(
                                observation_rx
                                    .recv_timeout(Duration::from_secs(30))
                                    .expect("Runtime issues separate Spool cleanup ACK"),
                                WorkerControl::SevenZipExtractBatchCleanupJournaledExact {
                                    token,
                                    batch: batch.clone(),
                                    item: ItemId(ledger.binding.item),
                                    target: crate::operations::runtime::SevenZipExtractCleanupTarget::Spool,
                                }
                            );
                            continue;
                        } else {
                            assert!(accepted_control.is_some());
                        }
                    } else {
                        assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                    }
                }
                Ok(ExecutorFact::Terminal { token, report }) => {
                    if replace_source_during_registration_ack {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Failed
                        );
                        assert_eq!(report.receipts.len(), 1);
                        assert_eq!(
                            report.receipts[0].outcome,
                            crate::operations::model::ItemOutcome::Failed
                        );
                        assert_eq!(
                            report.receipts[0].detail.as_deref(),
                            Some("7z source identity changed")
                        );
                        assert!(report.receipts[0].created_archive_path.is_none());
                        assert!(report.receipts[0].created_folder_path.is_none());
                        assert_eq!(
                            std::fs::read(&source).unwrap(),
                            replacement_bytes.expect("replacement was installed")
                        );
                        assert!(!temp.path().join(fixture_name).exists());
                        assert!(
                            worker_spool_path
                                .as_ref()
                                .is_some_and(|path| !path.exists())
                        );
                        assert!(worker_tree_path.as_ref().is_some_and(|path| !path.exists()));
                        assert!(
                            !archive_inspection_observed.load(Ordering::Acquire),
                            "archive inspection/helper entry was not reached"
                        );
                        assert_eq!(tree_delete_intents, 0);
                        assert!(!saw_item_intent, "source rejection precedes item intent");
                    } else if fail_published_write {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        assert_eq!(report.receipts.len(), 1);
                        assert_eq!(
                            report.receipts[0].outcome,
                            crate::operations::model::ItemOutcome::Completed
                        );
                        assert_eq!(
                            report.receipts[0].created_folder_path.as_deref(),
                            Some(temp.path().join(fixture_name).as_path())
                        );
                    } else if fail_publish_verification {
                        assert_eq!(
                            worker_progress_facts, 0,
                            "root reopen failure publishes no progress fact"
                        );
                    }
                    if replace_source_during_registration_ack {
                        // Registration was persisted and exactly ACKed before the worker rejected the reused path.
                    } else if let Some(fixture) = drain_fixture {
                        assert!(
                            drain_descendant_seen,
                            "terminal before descendant marker; report={report:#?}, marker={}, spool={:?}",
                            drain_marker.display(),
                            worker_spool_path
                        );
                        assert!(drain_completed.load(Ordering::Acquire));
                        assert!(drain_cancel_sent || matches!(fixture, DecodeDrainFixture::Error));
                        assert_eq!(tree_delete_intents, 0);
                        assert_eq!(
                            report.outcome,
                            match fixture {
                                DecodeDrainFixture::Cancel =>
                                    crate::operations::model::TerminalOutcome::Cancelled,
                                DecodeDrainFixture::Error =>
                                    crate::operations::model::TerminalOutcome::Failed,
                            }
                        );
                    } else if fail_published_write {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                    } else if expire_cleanup_after_root_identity {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        assert_eq!(tree_delete_intents, 1, "cleanup intent is not replayed");
                        assert!(!saw_spool_deleted);
                        assert!(!saw_tree_deleted);
                        assert!(!saw_discarded);
                        let tree = uncertain_tree_path.as_ref().unwrap();
                        assert!(tree.is_dir(), "expired cleanup retains the exact root");
                        assert!(std::fs::rename(tree, tree.with_extension("held")).is_err());
                        let spool = worker_spool_path.as_ref().unwrap();
                        assert!(
                            spool.is_file(),
                            "tree expiry retains dependent spool custody"
                        );
                        assert!(std::fs::rename(spool, spool.with_extension("held")).is_err());
                    } else if fail_materialize_cleanup {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Cancelled
                        );
                        assert_eq!(tree_delete_intents, 1, "tree intent is not replayed");
                    } else if fail_copy_deadline {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Failed
                        );
                        assert_eq!(tree_delete_intents, 1, "tree intent is not replayed");
                        assert!(!uncertain_tree_path.as_ref().unwrap().exists());
                        assert!(!worker_spool_path.as_ref().unwrap().exists());
                    } else if fail_close_uncertain {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        assert_eq!(tree_delete_intents, 1, "tree intent is not replayed");
                        let tree = uncertain_tree_path.as_ref().unwrap();
                        assert!(tree.is_dir());
                        assert!(std::fs::rename(tree, tree.with_extension("held")).is_err());
                        let spool = worker_spool_path.as_ref().unwrap();
                        assert!(spool.is_file());
                        assert!(std::fs::rename(spool, spool.with_extension("held")).is_err());
                    } else if fail_publish_cancel
                        || collide_publish_after_intent
                        || fail_publish_verification
                    {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        if fail_publish_verification {
                            assert!(report.receipts.iter().all(|receipt| {
                                receipt.outcome != crate::operations::model::ItemOutcome::Completed
                            }));
                            assert!(!cancellation.load(Ordering::Acquire));
                        }
                    } else if fail_publish_deadline {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                    } else if fail_tree_identity {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        let tree = uncertain_tree_path.as_ref().unwrap();
                        assert!(tree.is_dir(), "CREATE_NEW directory remains held");
                        assert!(
                            std::fs::rename(tree, tree.with_extension("held")).is_err(),
                            "held CREATE_NEW directory cannot be renamed before Terminal release"
                        );
                        let spool = worker_spool_path.as_ref().unwrap();
                        assert!(spool.is_file(), "owned spool remains in worker custody");
                        assert!(
                            std::fs::rename(spool, spool.with_extension("held")).is_err(),
                            "owned spool cannot be renamed before Terminal release"
                        );
                    } else if fail_tree_deadline {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Failed
                        );
                        assert!(!uncertain_tree_path.as_ref().unwrap().exists());
                        assert!(!worker_spool_path.as_ref().unwrap().exists());
                    } else if fail_decode_deadline {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Failed
                        );
                        assert!(!uncertain_tree_path.as_ref().unwrap().exists());
                        assert!(!worker_spool_path.as_ref().unwrap().exists());
                    } else if fail_publish_cancel {
                        let stage = expired_stage_path.as_ref().unwrap();
                        assert!(stage.is_dir());
                        assert!(std::fs::rename(stage, stage.with_extension("held")).is_err());
                    } else if collide_publish_after_intent {
                        let stage = expired_stage_path.as_ref().unwrap();
                        assert!(stage.is_dir());
                        assert!(std::fs::rename(stage, stage.with_extension("held")).is_err());
                        assert_eq!(
                            std::fs::read(foreign_collision_path.as_ref().unwrap()).unwrap(),
                            foreign_collision_bytes
                        );
                    } else if fail_publish_verification {
                        let output = temp.path().join(fixture_name);
                        let child = output.join("-dash-root").join("-name @ space.bin");
                        assert!(!cancellation.load(Ordering::Acquire));
                        assert!(output.is_dir());
                        assert!(
                            expired_stage_path
                                .as_ref()
                                .is_some_and(|stage| !stage.exists()),
                            "native rename moved the owned stage to the destination"
                        );
                        assert!(std::fs::rename(&output, output.with_extension("held")).is_err());
                        assert!(std::fs::write(&child, b"must remain pinned").is_err());
                    } else if collide_spool || collide_tree {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        let collision = foreign_collision_path.as_ref().unwrap();
                        assert_eq!(std::fs::read(collision).unwrap(), foreign_collision_bytes);
                        if collide_tree {
                            let spool = worker_spool_path.as_ref().unwrap();
                            assert!(spool.is_file());
                            assert!(std::fs::rename(spool, spool.with_extension("held")).is_err());
                        }
                    }
                    runtime.commit_fact(ExecutorFact::Terminal { token, report });
                    let (predecessors, terminal) = runtime
                        .seven_zip_extract_terminal_write_plan(token)
                        .expect("frozen terminal write plan");
                    if fail_published_write {
                        assert!(injected_published_failure);
                        assert!(
                            !runtime
                                .acknowledge_seven_zip_extract_terminal_record(token, &terminal),
                            "terminal ACK cannot release custody while failed predecessor remains"
                        );
                        let output = temp.path().join(fixture_name);
                        let child = output.join("-dash-root").join("-name @ space.bin");
                        assert!(output.is_dir());
                        assert!(child.is_file());
                        assert!(std::fs::rename(&output, output.with_extension("held")).is_err());
                        assert!(std::fs::write(&child, b"must remain pinned").is_err());
                        assert_eq!(
                            predecessors.len(),
                            1,
                            "failed Published snapshot repaired first"
                        );
                    } else if fail_publish_verification {
                        assert!(predecessors.is_empty());
                        let persisted: serde_json::Value = serde_json::from_slice(
                            &std::fs::read(&journal_path).expect("PublishIntent was durably ACKed"),
                        )
                        .unwrap();
                        let ledger =
                            &persisted["attempts"][0]["seven_zip_extract_batch"]["items"][0];
                        assert_eq!(ledger["phase"], "PublishIntent");
                        assert_eq!(ledger["tree_state"], "Owned");
                        assert!(ledger["publication_identity"].is_null());
                    } else if fail_publish_deadline {
                        assert!(predecessors.is_empty());
                        let output = temp.path().join(fixture_name);
                        let stage = expired_stage_path.as_ref().unwrap();
                        assert!(!output.exists(), "expired PublishIntent never renames");
                        assert!(stage.is_dir(), "unpublished stage remains pinned");
                        assert!(std::fs::rename(stage, stage.with_extension("held")).is_err());
                    } else if fail_materialize_cleanup
                        || fail_close_uncertain
                        || expire_cleanup_after_root_identity
                    {
                        assert!(predecessors.is_empty());
                    } else if fail_tree_identity {
                        assert!(predecessors.is_empty());
                    } else {
                        assert!(predecessors.is_empty(), "all batch writes drained in order");
                    }
                    for (failed_batch, record) in predecessors {
                        writer
                            .upsert_with_reply(record)
                            .recv_blocking()
                            .expect("journal writer replies")
                            .expect("failed predecessor snapshot repairs in order");
                        assert!(
                            runtime
                                .acknowledge_seven_zip_extract_repair_batch(token, &failed_batch)
                        );
                        if fail_published_write {
                            assert!(
                                !runtime.acknowledge_seven_zip_extract_batch(token, &failed_batch),
                                "old normal ACK is rejected after ordered repair"
                            );
                        }
                    }
                    writer
                        .upsert_with_reply(terminal.clone())
                        .recv_blocking()
                        .expect("journal writer replies")
                        .expect("terminal snapshot persists");
                    assert!(
                        runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal)
                    );
                    break;
                }
                Ok(fact) => {
                    assert!(
                        !injected_published_failure
                            || !matches!(&fact, ExecutorFact::Progress { .. }),
                        "no progress may follow failed Published persistence"
                    );
                    if matches!(&fact, ExecutorFact::Progress { .. }) {
                        worker_progress_facts += 1;
                    }
                    runtime.commit_fact(fact);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if drain_fixture.is_some() => {
                    if !drain_descendant_seen && drain_marker.is_file() {
                        let pid: u32 = std::fs::read_to_string(&drain_marker)
                            .unwrap()
                            .parse()
                            .unwrap();
                        assert!(
                            crate::archive_provider::worker_fixture_descendant_alive_for_test(pid)
                        );
                        assert!(!drain_completed.load(Ordering::Acquire));
                        assert!(
                            worker_spool_path
                                .as_ref()
                                .is_some_and(|path| path.is_file())
                        );
                        drain_descendant_seen = true;
                        if matches!(drain_fixture, Some(DecodeDrainFixture::Cancel)) {
                            assert!(runtime.cancel(token).is_none());
                            drain_cancel_sent = true;
                        }
                    }
                }
                Err(error) => panic!("worker did not produce its terminal fact: {error}"),
            }
        }
        worker.join().unwrap();
        if let Some(bridge) = control_bridge {
            bridge
                .join()
                .expect("test control bridge reaches terminal ACK");
        }
        drop(writer);
        if fail_publish_verification {
            let output = temp.path().join(fixture_name);
            let released = output.with_extension("released");
            std::fs::rename(&output, &released)
                .expect("exact terminal ACK releases retained published-root custody");
            std::fs::rename(&released, &output).unwrap();
        }
        if fail_close_uncertain {
            // 終端ACK後だけテスト側で隔離handleを閉じ、fixtureを後始末する。
            assert!(
                super::windows::extract_cleanup::close_worker_quarantined_handle_after_terminal_for_test(),
                "test driver closes the injected quarantine only after exact terminal release"
            );
        }

        let output = temp.path().join(fixture_name);
        if replace_source_during_registration_ack {
            assert!(!output.exists());
            assert!(
                worker_spool_path
                    .as_ref()
                    .is_some_and(|path| !path.exists())
            );
            assert!(worker_tree_path.as_ref().is_some_and(|path| !path.exists()));
            assert_eq!(std::fs::read(&source).unwrap(), replacement_bytes.unwrap());
            assert_eq!(
                std::fs::read(source.with_extension("captured-original.7z")).unwrap(),
                source_bytes
            );
            assert!(!archive_inspection_observed.load(Ordering::Acquire));
        } else if drain_fixture.is_some() {
            assert!(
                !output.exists(),
                "decoder failure/cancel precedes tree creation"
            );
            assert!(!worker_spool_path.as_ref().unwrap().exists());
        } else if fail_materialize_cleanup
            || fail_copy_deadline
            || expire_cleanup_after_root_identity
        {
            assert!(
                !output.exists(),
                "failed unpublished materialization stays private"
            );
            if expire_cleanup_after_root_identity {
                assert!(uncertain_tree_path.as_ref().unwrap().is_dir());
                assert!(worker_spool_path.as_ref().unwrap().is_file());
            }
            assert_eq!(
                tree_delete_intents, 1,
                "unknown child resumes use the same intent"
            );
        } else if fail_publish_deadline {
            assert!(!output.exists(), "expired PublishIntent leaves no output");
            assert!(expired_stage_path.as_ref().unwrap().is_dir());
        } else if collide_spool {
            assert!(
                !output.exists(),
                "spool collision never creates the output tree"
            );
            assert_eq!(
                std::fs::read(foreign_collision_path.as_ref().unwrap()).unwrap(),
                foreign_collision_bytes
            );
        } else if collide_tree {
            assert_eq!(
                std::fs::read(foreign_collision_path.as_ref().unwrap()).unwrap(),
                foreign_collision_bytes
            );
            assert!(
                !output.exists(),
                "tree collision never creates the published output"
            );
        } else if fail_tree_identity {
            assert!(!output.exists(), "uncertain staged root is never published");
            assert!(uncertain_tree_path.as_ref().unwrap().is_dir());
        } else if expire_cleanup_after_root_identity {
            assert!(!output.exists(), "pending cleanup never publishes");
            assert!(uncertain_tree_path.as_ref().unwrap().is_dir());
            assert!(worker_spool_path.as_ref().unwrap().is_file());
        } else if fail_close_uncertain {
            assert!(!output.exists(), "uncertain cleanup never publishes");
            assert!(!uncertain_tree_path.as_ref().unwrap().exists());
        } else if fail_publish_cancel {
            assert!(!output.exists(), "cancel after PublishIntent never renames");
            assert!(expired_stage_path.as_ref().unwrap().is_dir());
        } else if collide_publish_after_intent {
            assert!(
                output.is_dir(),
                "foreign output collision remains untouched"
            );
            assert_eq!(
                std::fs::read(foreign_collision_path.as_ref().unwrap()).unwrap(),
                foreign_collision_bytes
            );
            assert!(expired_stage_path.as_ref().unwrap().is_dir());
        } else if fail_tree_deadline {
            assert!(!output.exists(), "expired tree intent never creates output");
            assert!(!uncertain_tree_path.as_ref().unwrap().exists());
            assert!(!worker_spool_path.as_ref().unwrap().exists());
        } else if fail_decode_deadline {
            assert!(
                !output.exists(),
                "expired decode completion never creates a tree"
            );
            assert!(!uncertain_tree_path.as_ref().unwrap().exists());
            assert!(!worker_spool_path.as_ref().unwrap().exists());
        } else if drain_fixture.is_some() {
            assert!(
                !output.exists(),
                "decode cancellation/error never publishes output"
            );
            assert!(!uncertain_tree_path.as_ref().unwrap().exists());
            assert!(!worker_spool_path.as_ref().unwrap().exists());
        } else {
            assert_eq!(tree_snapshot(&output), tree_snapshot(&fixture_expected));
        }
        if !replace_source_during_registration_ack {
            assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        }
        let mut top_level = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        top_level.sort();
        let mut expected_top_level = vec![
            std::ffi::OsString::from(format!("{fixture_name}.7z")),
            std::ffi::OsString::from("worker-journal.json"),
            std::ffi::OsString::from("worker-journal.json.bak"),
        ];
        if replace_source_during_registration_ack {
            expected_top_level.push(std::ffi::OsString::from("mixed.captured-original.7z"));
        }
        if drain_fixture.is_some() {
            expected_top_level.push(std::ffi::OsString::from("helper-descendant.pid"));
        }
        if collide_spool || collide_tree {
            expected_top_level.push(
                foreign_collision_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
            if collide_tree {
                expected_top_level.push(
                    worker_spool_path
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_owned(),
                );
            }
        } else if fail_tree_identity {
            expected_top_level.push(
                uncertain_tree_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
            expected_top_level.push(
                worker_spool_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
        } else if fail_close_uncertain {
            assert!(
                uncertain_tree_path
                    .as_ref()
                    .is_some_and(|path| !path.exists())
            );
            expected_top_level.push(
                worker_spool_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
        } else if fail_publish_cancel || collide_publish_after_intent {
            expected_top_level.push(
                expired_stage_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
            if collide_publish_after_intent {
                expected_top_level.push(std::ffi::OsString::from(fixture_name));
            }
        } else if expire_cleanup_after_root_identity {
            expected_top_level.push(
                uncertain_tree_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
            expected_top_level.push(
                worker_spool_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
        } else if fail_tree_deadline || fail_decode_deadline || fail_copy_deadline {
            assert!(
                worker_spool_path
                    .as_ref()
                    .is_some_and(|path| !path.exists()),
                "authorized spool cleanup completed before terminal"
            );
        } else if drain_fixture.is_some() {
            // Helper cancellation/error happens before any output tree is published.
        } else if !replace_source_during_registration_ack
            && !fail_materialize_cleanup
            && !fail_publish_deadline
        {
            expected_top_level.push(std::ffi::OsString::from(fixture_name));
        } else if fail_publish_deadline {
            expected_top_level.push(
                expired_stage_path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_owned(),
            );
        }
        expected_top_level.sort();
        assert_eq!(
            top_level, expected_top_level,
            "no spool or staging artifacts remain"
        );
        let reopened =
            crate::operations::journal::JournalStore::open(journal_path.clone()).unwrap();
        assert_eq!(reopened.attempt_keys().len(), 1);
        if fail_materialize_cleanup || fail_published_write {
            assert!(reopened.recoveries().is_empty());
        } else if fail_publish_deadline
            || collide_spool
            || collide_tree
            || fail_tree_identity
            || fail_close_uncertain
            || expire_cleanup_after_root_identity
            || fail_publish_cancel
            || collide_publish_after_intent
            || fail_publish_verification
        {
            assert!(matches!(
                reopened.recoveries().as_slice(),
                [crate::operations::journal::RecoveryItem::RecoveryRequired(record)]
                    if record.state == crate::operations::journal::JournalState::RecoveryRequired
            ));
        } else {
            assert!(reopened.recoveries().is_empty());
        }
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(journal_path).unwrap()).unwrap();
        let attempt = &persisted["attempts"][0];
        if replace_source_during_registration_ack {
            let batch = &attempt["seven_zip_extract_batch"];
            assert_eq!(batch["sequence"], 1);
            assert_eq!(batch["items"].as_array().unwrap().len(), 0);
            let guard = expected_request.seven_zip_extract_guard.as_ref().unwrap();
            let (volume, file_index) = guard.sources[0].source_identity.windows_parts().unwrap();
            let descriptor_identity = &batch["envelope"]["items"][0]["source_identity"];
            assert_eq!(descriptor_identity["volume"], volume);
            assert_eq!(descriptor_identity["file_index"], file_index);
            let terminal_receipts = attempt["seven_zip_extract_receipts"].as_array().unwrap();
            assert_eq!(terminal_receipts.len(), 1);
            assert_eq!(terminal_receipts[0]["outcome"], "failed");
            assert!(terminal_receipts[0]["output"].is_null());
            assert!(terminal_receipts[0]["publication_identity"].is_null());
            assert!(attempt["created_archive_path"].is_null());
            assert_eq!(attempt["terminal_outcome"], "failed");
            assert_eq!(attempt["receipts_recorded"], true);
        } else if fail_published_write {
            let batch = &attempt["seven_zip_extract_batch"];
            let ledger = &batch["items"][0];
            let receipt = &attempt["seven_zip_extract_receipts"][0];
            assert_eq!(attempt["state"], "terminal");
            assert_eq!(attempt["terminal_outcome"], "succeeded");
            assert_eq!(attempt["receipts_recorded"], true);
            assert_eq!(ledger["phase"], "Published");
            assert_eq!(ledger["tree_state"], "Published");
            assert_eq!(
                attempt["seven_zip_extract_receipts"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(receipt["outcome"], "completed");
            assert_eq!(receipt["output"], batch["envelope"]["items"][0]["output"]);
            assert_eq!(
                receipt["publication_identity"],
                ledger["publication_identity"]
            );
            assert!(!receipt["publication_identity"].is_null());
        } else if fail_publish_verification {
            let batch = &attempt["seven_zip_extract_batch"];
            let ledger = &batch["items"][0];
            assert_eq!(ledger["phase"], "PublishIntent");
            assert_eq!(ledger["spool_state"], "Deleted");
            assert_eq!(ledger["tree_state"], "Owned");
            assert!(!ledger["spool_identity"].is_null());
            assert!(!ledger["tree_identity"].is_null());
            assert!(!ledger["spool_ready"].is_null());
            assert!(!ledger["tree_ready"].is_null());
            assert!(ledger["publication_identity"].is_null());
            assert_eq!(
                attempt["seven_zip_extract_receipts"]
                    .as_array()
                    .unwrap()
                    .len(),
                0,
                "root reopen failure records no Published receipt"
            );
            assert!(
                expired_stage_path
                    .as_ref()
                    .is_some_and(|stage| !stage.exists()),
                "terminal ACK does not restore the renamed stage path"
            );
            assert!(temp.path().join(fixture_name).is_dir());
        }
        if !replace_source_during_registration_ack {
            assert!(
                attempt["seven_zip_extract_batch"]["sequence"]
                    .as_u64()
                    .is_some_and(|sequence| sequence >= 2),
                "the recorded intent sequence must include the first mutating boundary"
            );
            let expected_extract_receipt_count = if fail_publish_verification { 0 } else { 1 };
            assert_eq!(
                attempt["seven_zip_extract_receipts"]
                    .as_array()
                    .unwrap()
                    .len(),
                expected_extract_receipt_count
            );
            assert_eq!(attempt["receipts_recorded"], true);
        }
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn spool_only_failure_holds_parent_and_spool_until_terminal_ack() {
        run_spool_only_worker_scenario(true, false, false, false, false, false, false, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn observed_spool_owned_after_cancel_records_then_cleans_without_forward_mutation() {
        run_spool_only_worker_scenario(false, true, false, false, false, false, false, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn observed_spool_intent_after_cancel_records_never_created_without_io() {
        run_spool_only_worker_scenario(false, false, true, false, false, false, false, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn exact_spool_intent_ack_then_local_cancel_records_never_created_without_io() {
        run_spool_only_worker_scenario(false, false, false, false, true, false, false, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn queued_control_only_cancel_during_spool_intent_ack_records_never_created() {
        run_spool_only_worker_scenario(false, false, false, false, false, true, false, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn post_create_spool_clone_failure_retains_original_handle_until_terminal_ack() {
        run_spool_only_worker_scenario(false, false, false, true, false, false, false, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn post_create_spool_identity_failure_retains_original_handle_until_terminal_ack() {
        run_spool_only_worker_scenario(false, false, false, false, false, false, true, false);
    }

    #[test]
    #[ignore = "requires a fresh release image, installed 7-Zip provider, and worker fixture bundle"]
    fn real_worker_expired_deadline_after_spool_intent_ack_records_never_created() {
        run_spool_only_worker_scenario(false, false, false, false, false, false, false, true);
    }

    fn run_spool_only_worker_scenario(
        inject_failure: bool,
        cancel_after_owned: bool,
        cancel_after_intent: bool,
        fail_working_clone: bool,
        cancel_after_exact_intent_ack: bool,
        cancel_control_only_during_intent_ack: bool,
        fail_spool_identity: bool,
        expire_after_spool_intent_ack: bool,
    ) {
        let fixture_root = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_WORKER_FIXTURE_ROOT")
                .expect("native runner supplies the hashed worker fixture root"),
        );
        let release = PathBuf::from(
            std::env::var_os("GPUI_FILER_V6_TEST_EXECUTABLE")
                .expect("native runner supplies the reviewed release image"),
        );
        let seven_zip = PathBuf::from(
            std::env::var_os("GPUI_FILER_7ZIP_TEST_EXECUTABLE")
                .expect("native runner supplies the installed 7-Zip executable"),
        );
        let source_bytes = std::fs::read(fixture_root.join("mixed.7z")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("destination-parent");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("mixed.7z");
        std::fs::write(&source, &source_bytes).unwrap();

        let provider = crate::archive_provider::ArchiveProviderOwner::default();
        let revision = provider.begin_discovery();
        let executable = crate::archive_provider::discover_from_path(
            Some(seven_zip.parent().unwrap().as_os_str().to_owned()),
            temp.path().to_path_buf(),
        )
        .unwrap();
        let binding = executable
            .execution_hints
            .operation_binding
            .clone()
            .unwrap();
        assert!(provider.commit_discovery(revision, Some(executable)));
        let guard =
            crate::operations::model::test_zip_create_guard(&source, &parent.join("unused-output"));
        let request = crate::operations::model::OperationRequest::extract_7z_many(
            vec![source.clone()],
            crate::operations::model::SevenZipExtractGuard {
                destination_parent_identity: guard.destination_parent_identity,
                sources: vec![crate::operations::model::SevenZipExtractSourceGuard {
                    source_identity: guard.source_identity,
                    source_kind: guard.source_kind,
                    entry_name: guard.entry_name,
                }],
                provider_revision: revision,
                provider_binding: binding,
            },
        )
        .unwrap();
        let image =
            crate::archive_provider::application_image::lease_explicit_path_for_test(&release)
                .unwrap();

        let journal_path = temp.path().join("spool-only-journal.json");
        let writer = crate::operations::journal::JournalWriter::start(
            crate::operations::journal::JournalStore::open(journal_path).unwrap(),
        );
        let mut runtime = crate::operations::runtime::OperationRuntime::default();
        let token = runtime.submit(request).unwrap();
        let start = runtime.start_next().unwrap();
        let attempt = start.seven_zip_extract_attempt.unwrap();
        let crate::operations::model::ExecutorCommand::Prepare { request, .. } = start.command
        else {
            panic!("runtime starts the captured extraction request");
        };
        let controls = start.controls;
        let cancellation = start.cancellation;
        let (fact_tx, fact_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::execute_with_options(
                token,
                &request,
                Some(attempt),
                Some(provider),
                Some(image),
                cancellation.as_ref(),
                &controls,
                &mut |fact| fact_tx.send(fact).unwrap(),
                WorkerOptions {
                    fail_after_spool_owned: inject_failure,
                    fail_after_spool_owned_ordinal: None,
                    fail_spool_working_clone_after_create: fail_working_clone,
                    fail_spool_identity_after_create: fail_spool_identity,
                    fail_tree_identity_after_create: false,
                    cancel_during_materialize_after_child: false,
                    forget_materializer_child_ids: false,
                    expire_publish_deadline_after_intent: false,
                    cancel_after_spool_intent_ack: cancel_after_exact_intent_ack,
                    expire_after_spool_intent_ack: expire_after_spool_intent_ack,
                    expire_after_tree_intent_ack: false,
                    expire_after_decode: false,
                    expire_during_materialize_after_child: false,
                    close_uncertain_tree_cleanup: false,
                    expire_tree_cleanup_after_identity: false,
                    cancel_after_publish_intent_ack: false,
                    fail_publish_verification_after_rename: false,
                    decode_drain_fixture: None,
                    decode_drain_marker: None,
                    decode_drain_completed: None,
                    archive_inspection_observed: None,
                    stale_provider_rejection_observed: None,
                    attempt_context_rejection_observed: None,
                    destination_parent_rejection_observed: None,
                    tree_cleanup_replay: None,
                },
            );
        });
        let mut spool_path = None;
        let mut saw_spool_owned = false;
        let mut saw_spool_never_created = false;
        loop {
            match fact_rx.recv_timeout(Duration::from_secs(120)) {
                Ok(ExecutorFact::SevenZipExtractBatchIntent { token, batch }) => {
                    spool_path = Some(
                        super::path_from_journal(&batch.envelope.items[0].spool)
                            .unwrap_or_else(|_| panic!("registered spool path is valid")),
                    );
                    if batch
                        .items
                        .first()
                        .is_some_and(|ledger| ledger.phase == Phase::SpoolOwned)
                    {
                        saw_spool_owned = true;
                    }
                    if batch.items.first().is_some_and(|ledger| {
                        ledger.phase == Phase::SpoolCreateIntent
                            && ledger.spool_state == ObjectState::NeverCreated
                    }) {
                        saw_spool_never_created = true;
                    }
                    runtime.commit_fact(ExecutorFact::SevenZipExtractBatchIntent {
                        token,
                        batch: batch.clone(),
                    });
                    let record = runtime
                        .claim_pending_seven_zip_extract_batch_write(token, &batch)
                        .unwrap_or_else(|| {
                            panic!(
                                "Runtime rejected live batch seq={} ledger={:?}",
                                batch.sequence,
                                batch.items.first(),
                            )
                        });
                    if (cancel_after_owned || cancel_after_intent)
                        && batch.items.first().is_some_and(|ledger| {
                            ledger.phase
                                == if cancel_after_owned {
                                    Phase::SpoolOwned
                                } else {
                                    Phase::SpoolCreateIntent
                                }
                        })
                    {
                        assert!(runtime.cancel(token).is_none());
                    }
                    if cancel_control_only_during_intent_ack
                        && batch
                            .items
                            .first()
                            .is_some_and(|ledger| ledger.phase == Phase::SpoolCreateIntent)
                    {
                        assert!(runtime.send_control_only_cancel_for_test(token));
                    }
                    writer
                        .upsert_with_reply(record)
                        .recv_blocking()
                        .expect("journal writer replies")
                        .expect("batch snapshot persists");
                    assert!(runtime.acknowledge_seven_zip_extract_batch(token, &batch));
                }
                Ok(ExecutorFact::Terminal { token, report }) => {
                    if expire_after_spool_intent_ack {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Failed
                        );
                        let spool = spool_path.as_ref().unwrap();
                        assert!(!saw_spool_owned, "expired deadline precedes CREATE_NEW");
                        assert!(saw_spool_never_created);
                        assert!(!spool.exists());
                        assert_eq!(
                            report.receipts[0].outcome,
                            crate::operations::model::ItemOutcome::Failed
                        );
                    } else if cancel_after_intent
                        || cancel_after_exact_intent_ack
                        || cancel_control_only_during_intent_ack
                    {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Cancelled
                        );
                        let spool = spool_path.as_ref().unwrap();
                        assert!(!saw_spool_owned, "CREATE_NEW was never reached");
                        assert!(saw_spool_never_created, "NeverCreated is durably recorded");
                        assert!(!spool.exists());
                    } else if fail_working_clone || fail_spool_identity {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        assert!(
                            !saw_spool_owned,
                            "clone/identity failed before SpoolOwned was recorded"
                        );
                        let spool = spool_path.as_ref().unwrap();
                        assert!(
                            spool.is_file(),
                            "CREATE_NEW returned before injected clone failure"
                        );
                        assert!(std::fs::rename(spool, spool.with_extension("held")).is_err());
                        assert!(std::fs::rename(&parent, temp.path().join("parent-held")).is_err());
                    } else if cancel_after_owned {
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Cancelled
                        );
                        let spool = spool_path.as_ref().unwrap();
                        assert!(!spool.exists());
                        assert!(!parent.join("mixed").exists());
                    } else {
                        let spool = spool_path.as_ref().unwrap();
                        assert_eq!(
                            report.outcome,
                            crate::operations::model::TerminalOutcome::Unknown
                        );
                        assert!(spool.is_file());
                        let moved_spool = spool.with_extension("held");
                        assert!(std::fs::rename(&spool, &moved_spool).is_err());
                        let moved_parent = temp.path().join("destination-parent-held");
                        assert!(std::fs::rename(&parent, &moved_parent).is_err());
                    }

                    runtime.commit_fact(ExecutorFact::Terminal { token, report });
                    let (predecessors, terminal) = runtime
                        .seven_zip_extract_terminal_write_plan(token)
                        .expect("frozen terminal write plan");
                    assert!(predecessors.is_empty());
                    writer
                        .upsert_with_reply(terminal.clone())
                        .recv_blocking()
                        .expect("journal writer replies")
                        .expect("terminal snapshot persists");
                    assert!(
                        runtime.acknowledge_seven_zip_extract_terminal_record(token, &terminal)
                    );
                    break;
                }
                Ok(fact) => {
                    runtime.commit_fact(fact);
                }
                Err(error) => panic!("worker did not reach terminal custody boundary: {error}"),
            }
        }
        worker.join().unwrap();
        drop(writer);
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert!(!parent.join("mixed").exists());
    }

    fn tree_snapshot(root: &std::path::Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn walk(
            root: &std::path::Path,
            current: &std::path::Path,
            output: &mut BTreeMap<PathBuf, Option<Vec<u8>>>,
        ) {
            let mut entries = std::fs::read_dir(current)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    output.insert(relative, None);
                    walk(root, &path, output);
                } else {
                    assert!(
                        kind.is_file(),
                        "fixture output contains only ordinary files/directories"
                    );
                    output.insert(relative, Some(std::fs::read(path).unwrap()));
                }
            }
        }

        let mut output = BTreeMap::new();
        walk(root, root, &mut output);
        output
    }
}
