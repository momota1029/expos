//! Application-wide authority for file-operation lifecycle and journal state.

use crate::shell::i18n::{self, Message, Translator};
use gpui::{Global, WeakEntity};
use std::{cell::Cell, collections::HashMap, rc::Rc};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_RECOVERY_DIAGNOSIS_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecoveryCopyActivationTicket {
    key: String,
    owner_incarnation: u64,
    generation: u64,
}

fn next_recovery_diagnosis_owner() -> u64 {
    NEXT_RECOVERY_DIAGNOSIS_OWNER
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
        .expect("recovery diagnosis owner incarnation exhausted")
}

#[cfg(test)]
type WorkerExecutorForTest = Box<
    dyn FnOnce(
            super::runtime::WorkerStart,
            super::transport::WorkerFactSender,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send,
>;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InspectionKindForTest {
    CrossVolumeMove,
    PublishedMove,
}

#[cfg(test)]
pub(crate) struct InspectionGateForTest {
    pub(crate) kind: InspectionKindForTest,
    pub(crate) entered: async_channel::Sender<()>,
    pub(crate) release: async_channel::Receiver<()>,
}

use super::{
    journal::{JournalAttempt, JournalStore, JournalWriter},
    model::OperationService,
    runtime::{OperationRuntime, UndoEntry},
};

/// Mutable operation facts shared by every Shell in one GPUI Application.
///
/// Shells may project this state, but never own a second runtime or journal
/// writer. The serial worker pump can therefore outlive its originating Shell.
pub struct OperationApplicationOwner {
    pub(crate) runtime: OperationRuntime,
    pub(crate) journal: Option<JournalWriter>,
    pub(crate) recovery_records: Vec<JournalAttempt>,
    pub(crate) journal_notice: Option<String>,
    pub(crate) shell_entities: Vec<WeakEntity<crate::shell::FilerShell>>,
    live_shells: Rc<Cell<usize>>,
    worker_in_flight: bool,
    active_worker: Option<super::model::AttemptToken>,
    directory_copy_undo_custody: super::executor::DirectoryCopyUndoCustodySlot,
    next_directory_copy_undo_ticket: u64,
    active_directory_copy_undo_ticket: Option<(u64, usize, super::model::DirectoryCopyUndoReceipt)>,
    next_directory_copy_group_undo_ticket: u64,
    active_directory_copy_group_undo_ticket: Option<(u64, usize, Vec<super::model::DirectoryCopyUndoReceipt>)>,
    native_drop_transfers: HashMap<
        super::model::AttemptToken,
        (gpui::FileDropTransferCompletion, Option<gpui::FileDropTransferOutcome>, bool),
    >,
    active_mixed_copy_undo_ticket: Option<(u64, usize, Vec<super::model::CopyUndoItem>)>,
    next_trash_group_undo_ticket: u64,
    active_trash_group_undo_ticket: Option<(u64, usize, Vec<std::path::PathBuf>)>,
    pending_copy_stage_custody: HashMap<
        super::model::AttemptToken,
        Vec<super::runtime::CopyStageCustody>,
    >,
    directory_move_review_generation: u64,
    directory_move_reviews: HashMap<super::model::AttemptToken, (u64, super::model::DirectoryMoveReviewState)>,
    directory_move_review_probes: HashMap<super::model::AttemptToken, (u64, String, super::model::DirectoryMoveReview, super::model::DirectoryMoveRootProbe)>,
    recovery_diagnosis_owner: u64,
    recovery_diagnosis_generation: u64,
    recovery_diagnoses: HashMap<String, (super::model::ReopenedDirectoryMoveTicket, JournalAttempt, super::model::ReopenedDirectoryMoveProbe, super::model::ReopenedDirectoryMoveDiagnosisState)>,
    reopened_copy_stage_reviews: HashMap<String, (super::model::ReopenedCopyStageTicket, JournalAttempt, super::model::ReopenedCopyStageProbe, super::model::ReopenedCopyStageReviewState)>,
    recovery_copy_activations: HashMap<
        String,
        (
            RecoveryCopyActivationTicket,
            JournalAttempt,
            Option<super::model::AttemptToken>,
        ),
    >,
    recovery_copy_admissions_pending: std::collections::HashSet<String>,
    recovery_record_discards_pending: std::collections::HashSet<String>,
    #[cfg(test)]
    worker_claims_reserved_for_test: bool,
    #[cfg(test)]
    worker_executor_for_test: Option<WorkerExecutorForTest>,
    #[cfg(test)]
    inspection_gates_for_test: std::collections::VecDeque<InspectionGateForTest>,
}

impl Global for OperationApplicationOwner {}

impl OperationApplicationOwner {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            runtime: OperationRuntime::new(OperationService::default()),
            journal: None,
            recovery_records: Vec::new(),
            journal_notice: None,
            shell_entities: Vec::new(),
            live_shells: Rc::new(Cell::new(0)),
            worker_in_flight: false,
            active_worker: None,
            directory_copy_undo_custody: super::executor::DirectoryCopyUndoCustodySlot::default(),
            next_directory_copy_undo_ticket: 0,
            active_directory_copy_undo_ticket: None,
            next_directory_copy_group_undo_ticket: 0,
            active_directory_copy_group_undo_ticket: None,
            native_drop_transfers: HashMap::new(),
            active_mixed_copy_undo_ticket: None,
            next_trash_group_undo_ticket: 0,
            active_trash_group_undo_ticket: None,
            pending_copy_stage_custody: HashMap::new(),
            directory_move_review_generation: 0,
            directory_move_reviews: HashMap::new(),
            directory_move_review_probes: HashMap::new(),
            recovery_diagnosis_owner: next_recovery_diagnosis_owner(),
            recovery_diagnosis_generation: 0,
            recovery_diagnoses: HashMap::new(),
            reopened_copy_stage_reviews: HashMap::new(),
            recovery_copy_activations: HashMap::new(),
            recovery_copy_admissions_pending: std::collections::HashSet::new(),
            recovery_record_discards_pending: std::collections::HashSet::new(),
            worker_claims_reserved_for_test: true,
            worker_executor_for_test: None,
            inspection_gates_for_test: std::collections::VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_journal_store(store: JournalStore) -> Self {
        Self::from_journal_store(Some(store), None)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_journal_store_and_workers(store: JournalStore) -> Self {
        let mut owner = Self::from_journal_store(Some(store), None);
        owner.worker_claims_reserved_for_test = false;
        owner
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_journal_store_and_workers_observed_writer(
        store: JournalStore,
    ) -> (
        Self,
        async_channel::Receiver<super::journal::JournalWriterObservationForTest>,
    ) {
        let mut observations = None;
        let mut owner = Self::from_journal_store_with_writer_factory(
            Some(store),
            None,
            |store| {
                let (writer, receiver) = JournalWriter::start_observed_for_test(store);
                observations = Some(receiver);
                writer
            },
        );
        owner.worker_claims_reserved_for_test = false;
        (owner, observations.expect("observed writer factory runs for a present store"))
    }

    pub fn open() -> Self {
        let startup_translator = Translator::new(i18n::system_locale());
        let (journal_store, journal_notice) = match JournalStore::open_for_app() {
            Ok(journal) => (journal, None),
            Err(error) => (
                None,
                Some(format!(
                    "{}: {error}",
                    startup_translator.text(Message::OperationJournalPreserved)
                )),
            ),
        };
        Self::from_journal_store(journal_store, journal_notice)
    }

    fn from_journal_store(
        journal_store: Option<JournalStore>,
        journal_notice: Option<String>,
    ) -> Self {
        Self::from_journal_store_with_writer_factory(journal_store, journal_notice, JournalWriter::start)
    }

    fn from_journal_store_with_writer_factory(
        journal_store: Option<JournalStore>,
        mut journal_notice: Option<String>,
        start_writer: impl FnOnce(JournalStore) -> JournalWriter,
    ) -> Self {
        let startup_translator = Translator::new(i18n::system_locale());
        let runtime = OperationRuntime::with_journal_keys(
            OperationService::default(),
            journal_store
                .as_ref()
                .into_iter()
                .flat_map(|journal| journal.attempt_keys()),
        );
        if !runtime.identity_available() {
            let identity_notice = startup_translator
                .text(Message::OperationIdentityUnavailable)
                .into_owned();
            journal_notice = Some(match journal_notice {
                Some(previous) => format!("{previous}; {identity_notice}"),
                None => identity_notice,
            });
        }
        let recovery_records = journal_store
            .as_ref()
            .map(|journal| {
                journal
                    .recoveries()
                    .into_iter()
                    .filter_map(|item| match item {
                        super::journal::RecoveryItem::RecoveryRequired(attempt) => Some(attempt),
                        super::journal::RecoveryItem::Unknown { .. } => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let journal = journal_store.map(start_writer);
        Self {
            runtime,
            journal,
            recovery_records,
            journal_notice,
            shell_entities: Vec::new(),
            live_shells: Rc::new(Cell::new(0)),
            worker_in_flight: false,
            active_worker: None,
            directory_copy_undo_custody: super::executor::DirectoryCopyUndoCustodySlot::default(),
            next_directory_copy_undo_ticket: 0,
            active_directory_copy_undo_ticket: None,
            next_directory_copy_group_undo_ticket: 0,
            active_directory_copy_group_undo_ticket: None,
            native_drop_transfers: HashMap::new(),
            active_mixed_copy_undo_ticket: None,
            next_trash_group_undo_ticket: 0,
            active_trash_group_undo_ticket: None,
            pending_copy_stage_custody: HashMap::new(),
            directory_move_review_generation: 0,
            directory_move_reviews: HashMap::new(),
            directory_move_review_probes: HashMap::new(),
            recovery_diagnosis_owner: next_recovery_diagnosis_owner(),
            recovery_diagnosis_generation: 0,
            recovery_diagnoses: HashMap::new(),
            reopened_copy_stage_reviews: HashMap::new(),
            recovery_copy_activations: HashMap::new(),
            recovery_copy_admissions_pending: std::collections::HashSet::new(),
            recovery_record_discards_pending: std::collections::HashSet::new(),
            #[cfg(test)]
            worker_claims_reserved_for_test: false,
            #[cfg(test)]
            worker_executor_for_test: None,
            #[cfg(test)]
            inspection_gates_for_test: std::collections::VecDeque::new(),
        }
    }

    pub(crate) fn begin_directory_move_review(
        &mut self,
        token: super::model::AttemptToken,
    ) -> Option<(u64, String, async_channel::Receiver<Result<Option<JournalAttempt>, String>>)> {
        let job = self.runtime.job(token)?;
        if !job.attempts.iter().any(|attempt| {
            attempt.token == token && attempt.state == super::model::AttemptState::Terminal
        }) {
            return None;
        }
        self.directory_move_review_generation = self.directory_move_review_generation.checked_add(1)?;
        let generation = self.directory_move_review_generation;
        self.directory_move_review_probes.remove(&token);
        let Some(snapshot) = self.runtime.journal_attempt(token) else {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return None;
        };
        let key = snapshot.key.clone();
        let Some(writer) = self.journal.as_ref() else {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return None;
        };
        self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Pending));
        if self.directory_move_reviews.len() > 32 {
            if let Some(oldest_key) = self.directory_move_reviews.keys().copied().find(|key| *key != token) {
                self.directory_move_reviews.remove(&oldest_key);
                self.directory_move_review_probes.remove(&oldest_key);
            }
        }
        Some((generation, key.clone(), writer.read_attempt(key)))
    }

    pub(crate) fn cancel_directory_move_review(&mut self, token: super::model::AttemptToken) {
        self.directory_move_reviews.remove(&token);
        self.directory_move_review_probes.remove(&token);
    }

    pub(crate) fn commit_directory_move_review(
        &mut self,
        token: super::model::AttemptToken,
        generation: u64,
        expected_key: &str,
        result: Result<Option<JournalAttempt>, String>,
    ) -> bool {
        let Some((current_generation, _)) = self.directory_move_reviews.get(&token) else { return false; };
        if *current_generation != generation
            || !matches!(self.directory_move_reviews.get(&token).map(|(_, state)| state), Some(super::model::DirectoryMoveReviewState::Pending))
            || self.directory_move_review_probes.contains_key(&token)
            || !self.runtime.job(token).is_some_and(|job| job.attempts.iter().any(|attempt|
                attempt.token == token && attempt.state == super::model::AttemptState::Terminal))
        {
            return false;
        }
        if self.runtime.journal_attempt(token).is_none_or(|attempt| attempt.key != expected_key) {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return true;
        }
        let attempt = match result {
            Ok(Some(attempt)) if attempt.key == expected_key => attempt,
            Ok(None) | Err(_) => {
                self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
                return true;
            }
            _ => {
                self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
                return true;
            }
        };
        // Review only projects the whole durable attempt when it contains one
        // ordinary directory ledger. Mixed and multi-item batches remain
        // unavailable until the read model can represent every ledger.
        let Some(ledger) = (attempt.kind == super::journal::JournalKind::Move
            && attempt.sources.len() == 1
            && attempt.destination.is_some()
            && attempt.directory_moves.len() == 1)
            .then(|| attempt.directory_moves.first())
            .flatten()
            .filter(|ledger| {
                ledger.item == 0
                    && attempt.sources.first() == Some(&ledger.source)
            })
        else {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return true;
        };
        let (Some(source), Some(destination), Some(parent)) = (
            ledger.source.to_path(),
            ledger.destination.to_path(),
            attempt.destination.as_ref().and_then(|path| path.to_path()),
        ) else {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return true;
        };
        let Some(expected_destination) = source.file_name().map(|name| parent.join(name)) else {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return true;
        };
        if destination != expected_destination {
            self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
            return true;
        }
        let replace = if let Some(replace) = ledger.replace.as_ref() {
            let (Some(stage), Some(backup)) = (replace.stage.to_path(), replace.backup.to_path()) else {
                self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
                return true;
            };
            if stage.parent() != destination.parent()
                || backup.parent() != destination.parent()
                || stage == destination
                || backup == destination
                || stage == backup
            {
                self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Unavailable));
                return true;
            }
            Some(super::model::DirectoryMoveReplaceReview {
                stage,
                backup,
                phase: replace.phase,
                backup_root_disposition_recorded: replace.backup_root_disposition_recorded,
                backup_cleanup_complete: replace.backup_cleanup_complete,
                backup_children_total: replace.backup_children.len(),
                backup_children_dispositions_recorded: replace.backup_children.iter().filter(|child| child.disposition_recorded).count(),
                pending_backup_intent: replace.backup_pending.is_some(),
                stage_identity: (!replace.stage_identity.is_empty()).then(|| replace.stage_identity.clone()),
                displaced_identity: (!replace.approved_target_identity.is_empty()).then(|| replace.approved_target_identity.clone()),
            })
        } else { None };
        let review = super::model::DirectoryMoveReview {
            attempt: token,
            journal_key: attempt.key.clone(),
            source,
            destination,
            children_total: ledger.children.len(),
            children_dispositions_recorded: ledger.children.iter().filter(|child| child.disposition_recorded).count(),
            pending_child: ledger.pending.is_some(),
            root_disposition_recorded: ledger.root.as_ref().is_some_and(|root| root.disposition_recorded),
            pending_root: ledger.root.as_ref().is_some_and(|root| root.pending.is_some()),
            cleanup_complete: ledger.cleanup_complete,
            source_root_identity: ledger.root.as_ref().and_then(|root| (!root.source_identity.is_empty()).then(|| root.source_identity.clone())),
            destination_root_identity: ledger.root.as_ref().and_then(|root| (!root.destination_identity.is_empty()).then(|| root.destination_identity.clone())),
            source_root_observation: super::model::CopyInspectionState::Indeterminate,
            destination_root_observation: super::model::CopyInspectionState::Indeterminate,
            replace: replace.clone(),
            stage_root_observation: super::model::CopyInspectionState::Indeterminate,
            backup_root_observation: super::model::CopyInspectionState::Indeterminate,
        };
        let probe = super::model::DirectoryMoveRootProbe {
            attempt: token,
            journal_key: attempt.key.clone(),
            source: review.source.clone(),
            destination: review.destination.clone(),
            expected_source_identity: review.source_root_identity.clone(),
            expected_destination_identity: match replace.as_ref() {
                Some(facts) => facts.stage_identity.clone(),
                None => review.destination_root_identity.clone(),
            },
            stage: replace.as_ref().map(|facts| facts.stage.clone()),
            backup: replace.as_ref().map(|facts| facts.backup.clone()),
            expected_stage_identity: replace.as_ref().and_then(|facts| facts.stage_identity.clone()),
            expected_backup_identity: replace.as_ref().and_then(|facts| facts.displaced_identity.clone()),
        };
        self.directory_move_review_probes.insert(token, (generation, attempt.key, review, probe));
        true
    }

    pub(crate) fn directory_move_root_probe(
        &self,
        token: super::model::AttemptToken,
        generation: u64,
        expected_key: &str,
    ) -> Option<super::model::DirectoryMoveRootProbe> {
        let (pending_generation, key, _, probe) = self.directory_move_review_probes.get(&token)?;
        (*pending_generation == generation && key == expected_key).then(|| probe.clone())
    }

    pub(crate) fn finish_directory_move_root_probe(
        &mut self,
        token: super::model::AttemptToken,
        generation: u64,
        expected_key: &str,
        observations: super::model::DirectoryMoveRootObservations,
    ) -> bool {
        let Some((current_generation, _)) = self.directory_move_reviews.get(&token) else { return false; };
        if *current_generation != generation
            || !matches!(self.directory_move_reviews.get(&token).map(|(_, state)| state), Some(super::model::DirectoryMoveReviewState::Pending))
            || !self.runtime.job(token).is_some_and(|job| job.attempts.iter().any(|attempt|
                attempt.token == token && attempt.state == super::model::AttemptState::Terminal))
            || self.runtime.journal_attempt(token).is_none_or(|attempt| attempt.key != expected_key)
        {
            return false;
        }
        let Some((pending_generation, key, mut review, probe)) = self.directory_move_review_probes.remove(&token) else { return false; };
        if pending_generation != generation || key != expected_key || probe.journal_key != expected_key || probe.attempt != token || review.journal_key != expected_key {
            self.directory_move_review_probes.insert(token, (pending_generation, key, review, probe));
            return false;
        }
        review.source_root_observation = if review.source_root_identity.is_some() { observations.source } else { super::model::CopyInspectionState::Indeterminate };
        let destination_identity_available = match review.replace.as_ref() {
            Some(facts) => facts.stage_identity.is_some(),
            None => review.destination_root_identity.is_some(),
        };
        review.destination_root_observation = if destination_identity_available { observations.destination } else { super::model::CopyInspectionState::Indeterminate };
        review.stage_root_observation = if review.replace.as_ref().and_then(|facts| facts.stage_identity.as_ref()).is_some() { observations.stage } else { super::model::CopyInspectionState::Indeterminate };
        review.backup_root_observation = if review.replace.as_ref().and_then(|facts| facts.displaced_identity.as_ref()).is_some() { observations.backup } else { super::model::CopyInspectionState::Indeterminate };
        self.directory_move_reviews.insert(token, (generation, super::model::DirectoryMoveReviewState::Available(review)));
        true
    }

    pub(crate) fn directory_move_review(
        &self,
        token: super::model::AttemptToken,
    ) -> Option<(u64, &super::model::DirectoryMoveReviewState)> {
        self.directory_move_reviews.get(&token).map(|(generation, state)| (*generation, state))
    }

    pub(crate) fn register_shell(
        &mut self,
        entity: WeakEntity<crate::shell::FilerShell>,
    ) -> OperationShellLease {
        // dead entryはfan-out対象から外し、Shellの再生成ごとに蓄積させない。
        self.shell_entities
            .retain(|shell| shell.upgrade().is_some());
        let count = self
            .live_shells
            .get()
            .checked_add(1)
            .expect("operation Shell lease count overflow");
        self.live_shells.set(count);
        self.shell_entities.push(entity);
        OperationShellLease {
            live_shells: self.live_shells.clone(),
        }
    }

    /// Commits a durable exact-key discard before reconciling disposable Shell projections.
    pub(crate) fn discard_recovery_record(&mut self, key: &str) -> bool {
        if self.recovery_copy_admissions_pending.contains(key)
            || self.recovery_record_discards_pending.contains(key)
        {
            return false;
        }
        let previous_len = self.recovery_records.len();
        self.recovery_records.retain(|record| record.key != key);
        let removed = self.recovery_records.len() != previous_len;
        if removed {
            self.recovery_diagnoses.remove(key);
            self.reopened_copy_stage_reviews.remove(key);
            self.recovery_copy_activations.remove(key);
            self.recovery_copy_admissions_pending.remove(key);
        }
        removed
    }

    pub(crate) fn recovery_copy_action_available(&self, snapshot: &JournalAttempt) -> bool {
        !self.recovery_copy_activations.contains_key(&snapshot.key)
            && !self.recovery_record_discards_pending.contains(&snapshot.key)
            && self.recovery_records.iter().any(|record| record == snapshot)
            && snapshot.kind == super::journal::JournalKind::Copy
            && snapshot.state == super::journal::JournalState::RecoveryRequired
            && !snapshot.sources.is_empty()
            && snapshot.sources.len() <= 1024
            && snapshot.sources.iter().all(|source| source.to_path().is_some_and(|path| path.file_name().is_some()))
            && snapshot.destination.as_ref().and_then(super::journal::JournalPath::to_path)
                .is_some_and(|path| path.file_name().is_some())
    }

    pub(crate) fn recovery_copy_admission_pending(&self, key: &str) -> bool {
        self.recovery_copy_admissions_pending.contains(key)
    }

    /// Recovery Copyのadmissionとdiscardが同じ履歴を競合更新しないよう予約する。
    pub(crate) fn begin_recovery_record_discard(&mut self, key: &str) -> bool {
        if self.recovery_copy_admissions_pending.contains(key)
            || !self.recovery_records.iter().any(|record| record.key == key)
        {
            return false;
        }
        self.recovery_record_discards_pending.insert(key.to_owned())
    }

    pub(crate) fn finish_recovery_record_discard(&mut self, key: &str, durable: bool) -> bool {
        if !self.recovery_record_discards_pending.remove(key) {
            return false;
        }
        durable && self.discard_recovery_record(key)
    }

    pub(crate) fn begin_recovery_copy_activation(
        &mut self,
        snapshot: &JournalAttempt,
    ) -> Option<(RecoveryCopyActivationTicket, Vec<std::path::PathBuf>, std::path::PathBuf)> {
        if !self.recovery_copy_action_available(snapshot) {
            return None;
        }
        let sources = snapshot.sources.iter().map(super::journal::JournalPath::to_path)
            .collect::<Option<Vec<_>>>()?;
        let destination = snapshot.destination.as_ref()?.to_path()?;
        if sources.iter().any(|source| source.file_name().is_none())
            || destination.file_name().is_none()
        {
            return None;
        }
        self.recovery_diagnosis_generation = self.recovery_diagnosis_generation.checked_add(1)?;
        let ticket = RecoveryCopyActivationTicket {
            key: snapshot.key.clone(),
            owner_incarnation: self.recovery_diagnosis_owner,
            generation: self.recovery_diagnosis_generation,
        };
        self.recovery_copy_activations
            .insert(snapshot.key.clone(), (ticket.clone(), snapshot.clone(), None));
        self.recovery_copy_admissions_pending
            .insert(snapshot.key.clone());
        Some((ticket, sources, destination))
    }

    pub(crate) fn cancel_recovery_copy_activation(&mut self, ticket: &RecoveryCopyActivationTicket) {
        let Some((current, _, pending_token)) = self.recovery_copy_activations.get(&ticket.key) else {
            return;
        };
        if current != ticket {
            return;
        }
        let pending_token = *pending_token;
        if let Some(token) = pending_token {
            let _ = self.runtime.reject_held(token);
        }
        self.recovery_copy_admissions_pending.remove(&ticket.key);
        self.recovery_copy_activations.remove(&ticket.key);
    }

    pub(crate) fn submit_recovery_copy_activation(
        &mut self,
        ticket: &RecoveryCopyActivationTicket,
        source_identities: Vec<crate::filesystem_identity::FilesystemIdentity>,
        destination_identity: crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<super::model::AttemptToken, super::model::SubmitError> {
        let Some((current, snapshot, pending_token)) =
            self.recovery_copy_activations.get(&ticket.key)
        else {
            return Err(super::model::SubmitError::InvalidRequest);
        };
        let sources = snapshot.sources.iter().map(super::journal::JournalPath::to_path)
            .collect::<Option<Vec<_>>>().ok_or(super::model::SubmitError::InvalidRequest)?;
        let destination = snapshot.destination.as_ref().and_then(super::journal::JournalPath::to_path)
            .ok_or(super::model::SubmitError::InvalidRequest)?;
        if current != ticket
            || pending_token.is_some()
            || ticket.owner_incarnation != self.recovery_diagnosis_owner
            || !self.recovery_records.iter().any(|record| record == snapshot)
            || source_identities.len() != sources.len()
            || source_identities.iter().any(|identity| identity.windows_parts().is_none())
            || destination_identity.windows_parts().is_none()
        {
            return Err(super::model::SubmitError::InvalidRequest);
        }
        let move_identity_guard = super::model::MoveIdentityGuard {

            source_identities,
            destination_identity: destination_identity.clone(),
            };
        let result = self.runtime.submit_held(super::model::OperationRequest {
            kind: super::model::OperationKind::Copy,
            virtual_source: None,
            sources,
            trash_source_guards: None,
            destination: Some(destination),
            batch_rename_plan: None,
            move_identity_guard: Some(move_identity_guard),
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
            source_cleanup_identity_guard: None,
            initial_conflict_policy: Some(super::model::ConflictDecision::KeepBoth),
            copy_destination_identity_guard: Some(destination_identity),
            create_parent_directory_identity_guard: None,
            zip_create_guard: None,
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        });
        match result {
            Ok(token) => {
                let Some((current, _, pending_token)) = self.recovery_copy_activations.get_mut(&ticket.key) else {
                    let _ = self.runtime.reject_held(token);
                    return Err(super::model::SubmitError::InvalidRequest);
                };
                if current != ticket || pending_token.replace(token).is_some() {
                    let _ = self.runtime.reject_held(token);
                    return Err(super::model::SubmitError::InvalidRequest);
                }
                Ok(token)
            }
            Err(error) => {
                self.cancel_recovery_copy_activation(ticket);
                Err(error)
            }
        }
    }

    /// 新しいRecovery Copyを履歴なしで実行しないよう、Queued recordのdurable ACK後にheld状態を解く。
    pub(crate) fn resolve_recovery_copy_admission(
        &mut self,
        ticket: &RecoveryCopyActivationTicket,
        token: super::model::AttemptToken,
        expected_record: Option<&super::journal::JournalAttempt>,
        acknowledged: bool,
    ) -> bool {
        let Some((current, snapshot, pending_token)) =
            self.recovery_copy_activations.get(&ticket.key)
        else {
            return false;
        };
        if current != ticket || *pending_token != Some(token) {
            return false;
        }
        let exact_pending = self.recovery_records.iter().any(|record| record == snapshot)
            && expected_record.is_some_and(|expected_record| {
                self.runtime.journal_attempt(token).is_some_and(|record| {
                    &record == expected_record
                        && record.state == super::journal::JournalState::Queued
                        && record.kind == super::journal::JournalKind::Copy
                        && record.key != snapshot.key
                        && record.sources == snapshot.sources
                        && record.destination == snapshot.destination
                })
            });
        if exact_pending && acknowledged && self.runtime.release_held(token) {
            self.recovery_copy_admissions_pending.remove(&ticket.key);
            return true;
        }

        let _ = self.runtime.reject_held(token);
        self.cancel_recovery_copy_activation(ticket);
        false
    }

    pub(crate) fn begin_reopened_copy_stage_review(
        &mut self,
        key: &str,
    ) -> Option<(super::model::ReopenedCopyStageTicket, super::model::ReopenedCopyStageProbe)> {
        self.reopened_copy_stage_reviews.remove(key);
        let record = self.recovery_records.iter().find(|record| record.key == key)?.clone();
        let historical = record.copy_directory_stage.as_ref()?;
        if record.kind != super::journal::JournalKind::Copy
            || record.state != super::journal::JournalState::RecoveryRequired
            || record.terminal_outcome != Some(super::journal::JournalTerminalOutcome::Unknown)
            || record.sources.len() != 1
            || historical.item != 0
            || historical.source != record.sources[0]
        { return None; }
        let source = historical.source.to_path()?;
        let destination = historical.destination.to_path()?;
        let stage = super::journal::JournalPath { units: historical.evidence.stage_path_units.clone() }.to_path()?;
        let expected_destination = record.destination.as_ref()?.to_path()?.join(source.file_name()?);
        if destination != expected_destination || stage == destination || stage.parent() != destination.parent() { return None; }
        self.recovery_diagnosis_generation = self.recovery_diagnosis_generation.checked_add(1)?;
        let ticket = super::model::ReopenedCopyStageTicket { key: key.to_owned(), owner_incarnation: self.recovery_diagnosis_owner, generation: self.recovery_diagnosis_generation };
        let expected_stage_identity = if historical.evidence.phase == super::model::CopyDirectoryStagePhase::CreatedRootIdentityUnverified {
            None
        } else {
            (!historical.evidence.expected_root_identity.is_empty()).then(|| historical.evidence.expected_root_identity.clone())
        };
        let probe = super::model::ReopenedCopyStageProbe {
            stage,
            destination,
            expected_stage_identity,
        };
        self.reopened_copy_stage_reviews.insert(key.to_owned(), (ticket.clone(), record, probe.clone(), super::model::ReopenedCopyStageReviewState::Pending));
        Some((ticket, probe))
    }

    pub(crate) fn cancel_reopened_copy_stage_review(&mut self, key: &str, ticket: &super::model::ReopenedCopyStageTicket) {
        if ticket.key == key && self.reopened_copy_stage_reviews.get(key).is_some_and(|(current, _, _, _)| current == ticket) {
            self.reopened_copy_stage_reviews.remove(key);
        }
    }

    pub(crate) fn commit_reopened_copy_stage_review(
        &mut self,
        key: &str,
        ticket: &super::model::ReopenedCopyStageTicket,
        observations: super::model::ReopenedCopyStageObservations,
    ) -> bool {
        let Some((current, snapshot, probe, state)) = self.reopened_copy_stage_reviews.get(key) else { return false; };
        if ticket.key != key || current != ticket || ticket.owner_incarnation != self.recovery_diagnosis_owner
            || !matches!(state, super::model::ReopenedCopyStageReviewState::Pending)
            || !self.recovery_records.iter().any(|record| record == snapshot)
        { return false; }
        let Some(historical) = snapshot.copy_directory_stage.clone() else { return false; };
        let Some(source) = historical.source.to_path() else { return false; };
        let review = super::model::ReopenedCopyStageReview {
            source,
            destination: probe.destination.clone(),
            historical,
            stage_root_observation: if probe.expected_stage_identity.is_some() { observations.stage } else { super::model::CopyInspectionState::Indeterminate },
            destination_root_observation: super::model::CopyInspectionState::Indeterminate,
        };
        self.reopened_copy_stage_reviews.get_mut(key).expect("review ticket exists").3 = super::model::ReopenedCopyStageReviewState::Available(review);
        true
    }

    pub(crate) fn reopened_copy_stage_review(&self, key: &str) -> Option<(super::model::ReopenedCopyStageTicket, &super::model::ReopenedCopyStageReviewState)> {
        let (ticket, snapshot, _, state) = self.reopened_copy_stage_reviews.get(key)?;
        self.recovery_records.iter().any(|record| record == snapshot).then(|| (ticket.clone(), state))
    }

    pub(crate) fn begin_reopened_directory_move_diagnosis(
        &mut self,
        key: &str,
    ) -> Option<(super::model::ReopenedDirectoryMoveTicket, super::model::ReopenedDirectoryMoveProbe)> {
        self.recovery_diagnoses.remove(key);
        let record = self.recovery_records.iter().find(|record| record.key == key)?.clone();
        if record.kind != super::journal::JournalKind::Move || record.state != super::journal::JournalState::RecoveryRequired
            || record.sources.len() != 1 || record.directory_moves.len() != 1 { return None; }
        let ledger = record.directory_moves.first()?;
        if ledger.item != 0 || ledger.source != record.sources[0] || ledger.replace.is_some() { return None; }
        let (source, destination, parent) = (ledger.source.to_path()?, ledger.destination.to_path()?, record.destination.as_ref()?.to_path()?);
        if source != record.sources[0].to_path()? || destination != parent.join(source.file_name()?) { return None; }
        self.recovery_diagnosis_generation = self.recovery_diagnosis_generation.checked_add(1)?;
        let ticket = super::model::ReopenedDirectoryMoveTicket { key: key.to_owned(), owner_incarnation: self.recovery_diagnosis_owner, generation: self.recovery_diagnosis_generation };
        let probe = super::model::ReopenedDirectoryMoveProbe { source, destination, expected_source_identity: ledger.root.as_ref().and_then(|root| (!root.source_identity.is_empty()).then(|| root.source_identity.clone())), expected_destination_identity: ledger.root.as_ref().and_then(|root| (!root.destination_identity.is_empty()).then(|| root.destination_identity.clone())) };
        self.recovery_diagnoses.insert(key.to_owned(), (ticket.clone(), record, probe.clone(), super::model::ReopenedDirectoryMoveDiagnosisState::Pending));
        Some((ticket, probe))
    }

    pub(crate) fn cancel_reopened_directory_move_diagnosis(&mut self, key: &str, ticket: &super::model::ReopenedDirectoryMoveTicket) {
        if ticket.key == key && self.recovery_diagnoses.get(key).is_some_and(|(current, _, _, _)| current == ticket) { self.recovery_diagnoses.remove(key); }
    }

    pub(crate) fn commit_reopened_directory_move_diagnosis(
        &mut self, key: &str, ticket: &super::model::ReopenedDirectoryMoveTicket, observations: super::model::ReopenedDirectoryMoveObservations,
    ) -> bool {
        let Some((current, snapshot, probe, state)) = self.recovery_diagnoses.get(key) else { return false; };
        if ticket.key != key || current != ticket || ticket.owner_incarnation != self.recovery_diagnosis_owner || !matches!(state, super::model::ReopenedDirectoryMoveDiagnosisState::Pending)
            || !self.recovery_records.iter().any(|record| record == snapshot) { return false; }
        let diagnosis = super::model::ReopenedDirectoryMoveDiagnosis {
            source: probe.source.clone(), destination: probe.destination.clone(),
            source_root_identity: probe.expected_source_identity.clone(), destination_root_identity: probe.expected_destination_identity.clone(),
            source_root_observation: if probe.expected_source_identity.is_some() { observations.source } else { super::model::CopyInspectionState::Indeterminate },
            destination_root_observation: if probe.expected_destination_identity.is_some() { observations.destination } else { super::model::CopyInspectionState::Indeterminate },
        };
        self.recovery_diagnoses.get_mut(key).expect("diagnosis exists").3 = super::model::ReopenedDirectoryMoveDiagnosisState::Available(diagnosis);
        true
    }

    pub(crate) fn reopened_directory_move_diagnosis(&self, key: &str) -> Option<(super::model::ReopenedDirectoryMoveTicket, &super::model::ReopenedDirectoryMoveDiagnosisState)> {
        let (ticket, snapshot, _, state) = self.recovery_diagnoses.get(key)?;
        self.recovery_records.iter().any(|record| record == snapshot).then(|| (ticket.clone(), state))
    }

    /// Claims the serial lane until the executor task has actually returned.
    pub(crate) fn claim_next_worker(&mut self) -> Option<super::runtime::WorkerStart> {
        #[cfg(test)]
        if self.worker_claims_reserved_for_test {
            // UI routing tests inspect accepted requests; never let synthetic paths reach the executor.
            return None;
        }
        if self.live_shells.get() == 0 || self.worker_in_flight {
            return None;
        }
        let worker = self.runtime.start_next()?;
        self.worker_in_flight = true;
        self.active_worker = match &worker.command {
            super::model::ExecutorCommand::Prepare { token, .. } => Some(*token),
            _ => None,
        };
        Some(worker)
    }

    /// Receives a failed stage capability into bounded, untrusted-until-return custody.
    pub(crate) fn retain_pending_copy_stage_custody(
        &mut self,
        custody: super::runtime::CopyStageCustody,
    ) -> bool {
        if !self.worker_in_flight || self.active_worker != Some(custody.token) {
            return false;
        }
        let pending = self
            .pending_copy_stage_custody
            .entry(custody.token)
            .or_default();
        if pending.len() >= 64 || pending.iter().any(|item| item.item == custody.item) {
            return false;
        }
        pending.push(custody);
        true
    }

    /// Terminal facts can clear runtime.running before the executor unwinds.
    /// Keep the app-level gate closed until its JoinHandle returns.
    pub(crate) fn worker_returned_and_claim_next(
        &mut self,
        returned_attempt: super::model::AttemptToken,
    ) -> (
        Option<super::runtime::WorkerStart>,
        Option<super::model::GuardedMoveInspectionResult>,
        Option<super::model::CrossVolumeMoveInspectionResult>,
        Option<super::model::PublishedMoveInspectionResult>,
        Option<super::model::CopyFailureInspectionResult>,
    ) {
        assert!(
            self.worker_in_flight,
            "operation worker returned without an active lane"
        );
        assert_eq!(self.active_worker, Some(returned_attempt));
        self.active_worker = None;
        self.worker_in_flight = false;
        self.runtime
            .mark_operation_worker_returned(returned_attempt);
        self.note_native_drop_worker_returned(returned_attempt);
        if let Some(custodies) = self.pending_copy_stage_custody.remove(&returned_attempt) {
            for custody in custodies {
                let _ = self.runtime.adopt_returned_copy_stage_custody(custody);
            }
        }
        if let Some(inspection) = self.runtime.begin_guarded_move_inspection(returned_attempt) {
            // The same serial lane stays occupied until this detached read-only
            // inspection returns and its exact ticket is committed.
            self.worker_in_flight = true;
            (None, Some(inspection), None, None, None)
        } else if let Some(inspection) = self.runtime.begin_cross_volume_move_inspection(returned_attempt) {
            self.worker_in_flight = true;
            (None, None, Some(inspection), None, None)
        } else if let Some(inspection) = self.runtime.begin_published_move_inspection(returned_attempt) {
            self.worker_in_flight = true;
            (None, None, None, Some(inspection), None)
        } else if let Some(inspection) =
            self.runtime.begin_copy_failure_inspection(returned_attempt)
        {
            self.worker_in_flight = true;
            (None, None, None, None, Some(inspection))
        } else {
            (self.claim_next_worker(), None, None, None, None)
        }
    }

    pub(crate) fn bind_native_drop_transfer(
        &mut self,
        token: super::model::AttemptToken,
        completion: gpui::FileDropTransferCompletion,
    ) -> bool {
        use std::collections::hash_map::Entry;
        match self.native_drop_transfers.entry(token) {
            Entry::Vacant(entry) => {
                entry.insert((completion, None, false));
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    pub(crate) fn note_native_drop_terminal(
        &mut self,
        token: super::model::AttemptToken,
        outcome: gpui::FileDropTransferOutcome,
    ) {
        let worker_returned = if let Some((_, terminal, worker_returned)) = self.native_drop_transfers.get_mut(&token) {
            if terminal.is_none() {
                *terminal = Some(outcome);
            }
            *worker_returned
        } else {
            false
        };
        if worker_returned {
            self.settle_native_drop_transfer(token);
        }
    }

    fn note_native_drop_worker_returned(&mut self, token: super::model::AttemptToken) {
        let terminal_observed = if let Some((_, terminal, worker_returned)) = self.native_drop_transfers.get_mut(&token) {
            *worker_returned = true;
            terminal.is_some()
        } else {
            false
        };
        if terminal_observed {
            self.settle_native_drop_transfer(token);
        }
    }

    fn settle_native_drop_transfer(&mut self, token: super::model::AttemptToken) {
        let ready = self
            .native_drop_transfers
            .get(&token)
            .is_some_and(|(_, outcome, worker_returned)| outcome.is_some() && *worker_returned);
        if ready {
            if let Some((completion, Some(outcome), _)) = self.native_drop_transfers.remove(&token) {
                completion.complete(outcome);
            }
        }
    }

    /// Undo shares the operation owner's serial filesystem lane. A terminal
    /// fact may already be in Runtime while its journal ACK is still pending.
    pub(crate) fn reserve_undo(&mut self) -> Option<(usize, UndoEntry)> {
        if self.worker_in_flight
            || (self.directory_copy_undo_custody.is_occupied()
                && self.runtime.undo_top_is_directory_copy())
        {
            return None;
        }
        let reserved = self.runtime.reserve_undo()?;
        if let UndoEntry::TrashGroup(receipts) = &reserved.1 {
            self.next_trash_group_undo_ticket = self.next_trash_group_undo_ticket.wrapping_add(1).max(1);
            let paths = receipts.iter().map(|receipt| receipt.original_path()).collect::<Vec<_>>();
            self.active_trash_group_undo_ticket = Some((self.next_trash_group_undo_ticket, reserved.0, paths));
            self.active_directory_copy_undo_ticket = None;
            self.active_directory_copy_group_undo_ticket = None;
            self.active_mixed_copy_undo_ticket = None;
        } else if let UndoEntry::DirectoryCopy(receipt) = &reserved.1 {
            self.next_directory_copy_undo_ticket = self.next_directory_copy_undo_ticket.wrapping_add(1).max(1);
            self.active_directory_copy_undo_ticket = Some((
                self.next_directory_copy_undo_ticket,
                reserved.0,
                receipt.clone(),
            ));
            self.active_directory_copy_group_undo_ticket = None;
            self.active_mixed_copy_undo_ticket = None;
            self.active_trash_group_undo_ticket = None;
        } else if let UndoEntry::DirectoryCopyGroup(receipts) = &reserved.1 {
            self.next_directory_copy_group_undo_ticket = self.next_directory_copy_group_undo_ticket.wrapping_add(1).max(1);
            self.active_directory_copy_group_undo_ticket = Some((self.next_directory_copy_group_undo_ticket, reserved.0, receipts.clone()));
            self.active_directory_copy_undo_ticket = None;
            self.active_mixed_copy_undo_ticket = None;
            self.active_trash_group_undo_ticket = None;
        } else if let UndoEntry::MixedCopyGroup(items) = &reserved.1 {
            self.next_directory_copy_group_undo_ticket = self.next_directory_copy_group_undo_ticket.wrapping_add(1).max(1);
            self.active_mixed_copy_undo_ticket = Some((self.next_directory_copy_group_undo_ticket, reserved.0, items.clone()));
            self.active_directory_copy_undo_ticket = None;
            self.active_directory_copy_group_undo_ticket = None;
            self.active_trash_group_undo_ticket = None;
        } else {
            self.active_directory_copy_undo_ticket = None;
            self.active_directory_copy_group_undo_ticket = None;
            self.active_mixed_copy_undo_ticket = None;
            self.active_trash_group_undo_ticket = None;
        }
        Some(reserved)
    }

    pub(crate) fn directory_copy_undo_custody_slot(
        &self,
    ) -> super::executor::DirectoryCopyUndoCustodySlot {
        self.directory_copy_undo_custody.clone()
    }

    pub(crate) fn directory_copy_undo_ticket(
        &self,
        reservation: usize,
        receipt: &super::model::DirectoryCopyUndoReceipt,
    ) -> Option<u64> {
        self.active_directory_copy_undo_ticket.as_ref().and_then(|(ticket, index, expected)| {
            (*index == reservation && expected == receipt
                && self.runtime.undo_entry_matches_directory_copy(reservation, receipt))
                .then_some(*ticket)
        })
    }

    pub(crate) fn finish_directory_copy_undo(
        &mut self,
        ticket: u64,
        reservation: usize,
        receipt: &super::model::DirectoryCopyUndoReceipt,
        consume: bool,
    ) -> bool {
        let is_current = self.active_directory_copy_undo_ticket.as_ref().is_some_and(
            |(actual, index, expected)| {
                *actual == ticket && *index == reservation && expected == receipt
                    && self.runtime.undo_entry_matches_directory_copy(reservation, receipt)
            },
        );
        if !is_current || !self.runtime.finish_trash_undo(reservation, consume) {
            return false;
        }
        self.active_directory_copy_undo_ticket = None;
        true
    }

    pub(crate) fn directory_copy_group_undo_ticket(&self, reservation: usize, receipts: &[super::model::DirectoryCopyUndoReceipt]) -> Option<u64> {
        self.active_directory_copy_group_undo_ticket.as_ref().and_then(|(ticket, index, expected)| {
            (*index == reservation && expected == receipts && self.runtime.undo_entry_matches_directory_copy_group(reservation, receipts)).then_some(*ticket)
        })
    }

    pub(crate) fn finish_directory_copy_group_undo(&mut self, ticket: u64, reservation: usize, receipts: &[super::model::DirectoryCopyUndoReceipt], consumed_reverse_suffix: usize) -> bool {
        let current = self.active_directory_copy_group_undo_ticket.as_ref().is_some_and(|(actual, index, expected)| {
            *actual == ticket && *index == reservation && expected == receipts && self.runtime.undo_entry_matches_directory_copy_group(reservation, receipts)
        });
        if !current || !self.runtime.finish_directory_copy_group_undo(reservation, consumed_reverse_suffix) { return false; }
        self.active_directory_copy_group_undo_ticket = None;
        true
    }

    pub(crate) fn mixed_copy_undo_ticket(&self, reservation: usize, items: &[super::model::CopyUndoItem]) -> Option<u64> {
        self.active_mixed_copy_undo_ticket.as_ref().and_then(|(ticket, index, expected)| {
            (*index == reservation && expected == items && self.runtime.undo_entry_matches_mixed_copy_group(reservation, items)).then_some(*ticket)
        })
    }

    pub(crate) fn finish_mixed_copy_undo(&mut self, ticket: u64, reservation: usize, items: &[super::model::CopyUndoItem], consumed_prefix: usize) -> bool {
        let current = self.active_mixed_copy_undo_ticket.as_ref().is_some_and(|(actual, index, expected)| {
            *actual == ticket && *index == reservation && expected == items && self.runtime.undo_entry_matches_mixed_copy_group(reservation, items)
        });
        if !current || !self.runtime.finish_mixed_copy_group_undo(reservation, consumed_prefix) { return false; }
        self.active_mixed_copy_undo_ticket = None;
        true
    }

    pub(crate) fn trash_group_undo_ticket(&self, reservation: usize, original_paths: &[std::path::PathBuf]) -> Option<u64> {
        self.active_trash_group_undo_ticket.as_ref().and_then(|(ticket, index, expected)| {
            (*index == reservation && expected == original_paths && self.runtime.undo_entry_matches_trash_group(reservation, original_paths)).then_some(*ticket)
        })
    }

    pub(crate) fn finish_trash_group_undo(&mut self, ticket: u64, reservation: usize, original_paths: &[std::path::PathBuf], consumed_reverse_suffix: usize) -> bool {
        let current = self.active_trash_group_undo_ticket.as_ref().is_some_and(|(actual, index, expected)| {
            *actual == ticket && *index == reservation && expected == original_paths && self.runtime.undo_entry_matches_trash_group(reservation, original_paths)
        });
        if !current || !self.runtime.finish_trash_group_undo(reservation, original_paths, consumed_reverse_suffix) { return false; }
        self.active_trash_group_undo_ticket = None;
        true
    }

    pub(crate) fn cross_volume_move_inspection_worker_returned_and_claim_next(
        &mut self,
        result: super::model::CrossVolumeMoveInspectionResult,
    ) -> (
        Option<super::runtime::WorkerStart>,
        Option<super::model::PublishedMoveInspectionResult>,
    ) {
        assert!(self.worker_in_flight, "cross-volume Move inspection returned without an active lane");
        let attempt = result.attempt;
        self.runtime.finish_cross_volume_move_inspection(result);
        if let Some(inspection) = self.runtime.begin_published_move_inspection(attempt) {
            // A multi-item Move can have both a pre-publication failure and a
            // separate item whose destination was published while its source
            // remained. Keep the same serial lane until both passive reviews finish.
            return (None, Some(inspection));
        }
        self.worker_in_flight = false;
        (self.claim_next_worker(), None)
    }

    pub(crate) fn published_move_inspection_worker_returned_and_claim_next(
        &mut self,
        result: super::model::PublishedMoveInspectionResult,
    ) -> Option<super::runtime::WorkerStart> {
        assert!(self.worker_in_flight, "published Move inspection returned without an active lane");
        self.runtime.finish_published_move_inspection(result);
        self.worker_in_flight = false;
        self.claim_next_worker()
    }

    pub(crate) fn inspection_worker_returned_and_claim_next(
        &mut self,
        result: super::model::GuardedMoveInspectionResult,
    ) -> Option<super::runtime::WorkerStart> {
        assert!(
            self.worker_in_flight,
            "inspection returned without an active lane"
        );
        self.runtime.finish_guarded_move_inspection(result);
        self.worker_in_flight = false;
        self.claim_next_worker()
    }

    pub(crate) fn copy_inspection_worker_returned_and_claim_next(
        &mut self,
        result: super::model::CopyFailureInspectionResult,
    ) -> Option<super::runtime::WorkerStart> {
        assert!(
            self.worker_in_flight,
            "copy inspection returned without an active lane"
        );
        self.runtime.finish_copy_failure_inspection(result);
        self.worker_in_flight = false;
        self.claim_next_worker()
    }

    #[cfg(test)]
    pub(crate) fn worker_claimed_for_test(&self) -> bool {
        self.worker_in_flight
    }

    #[cfg(test)]
    pub(crate) fn live_shell_count_for_test(&self) -> usize {
        self.live_shells.get()
    }

    #[cfg(test)]
    pub(crate) fn configure_worker_pump_test(
        &mut self,
        executor: impl FnOnce(
            super::runtime::WorkerStart,
            super::transport::WorkerFactSender,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + 'static,
    ) {
        self.worker_claims_reserved_for_test = false;
        self.worker_executor_for_test = Some(Box::new(executor));
    }

    #[cfg(test)]
    pub(crate) fn take_worker_executor_for_test(&mut self) -> Option<WorkerExecutorForTest> {
        self.worker_executor_for_test.take()
    }

    #[cfg(test)]
    pub(crate) fn set_inspection_gates_for_test(
        &mut self,
        gates: impl IntoIterator<Item = InspectionGateForTest>,
    ) {
        self.inspection_gates_for_test = gates.into_iter().collect();
    }

    #[cfg(test)]
    pub(crate) fn take_inspection_gate_for_test(
        &mut self,
        kind: InspectionKindForTest,
    ) -> Option<InspectionGateForTest> {
        let index = self
            .inspection_gates_for_test
            .iter()
            .position(|gate| gate.kind == kind)?;
        self.inspection_gates_for_test.remove(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_move_review_rejects_delayed_older_generation() {
        let mut owner = OperationApplicationOwner::for_test();
        let token = super::super::model::AttemptToken::fixture();
        owner.directory_move_reviews.insert(token, (9, super::super::model::DirectoryMoveReviewState::Pending));
        assert!(!owner.commit_directory_move_review(
            token,
            8,
            "r:old:attempt",
            Err("delayed test response".into()),
        ));
        assert_eq!(owner.directory_move_reviews.get(&token).unwrap().0, 9);
        assert_eq!(owner.directory_move_reviews.get(&token).unwrap().1, super::super::model::DirectoryMoveReviewState::Pending);
    }

    #[test]
    fn directory_move_root_probe_requires_exact_generation_key_and_live_attempt() {
        use super::super::model::{
            CopyInspectionState as State, DirectoryMoveReview, DirectoryMoveRootProbe,
            DirectoryMoveRootObservations, DirectoryMoveReviewState, ItemId, ItemOutcome,
            ItemReceipt, ProgressCounters, RetryPlan, TerminalOutcome, TerminalReport,
        };

        let make_owner = || {
            let mut owner = OperationApplicationOwner::for_test();
            let mut request = copy_request("C:/source/dir");
            request.kind = super::super::model::OperationKind::Move;
            request.destination = Some("E:/target".into());
            let token = owner.runtime.submit(request).unwrap();
            owner.runtime.start_next().unwrap();
            owner.runtime.commit_fact(super::super::executor::ExecutorFact::Terminal {
                token,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    final_counters: ProgressCounters::default(),
                    receipts: vec![ItemReceipt {
                        item: ItemId(0), outcome: ItemOutcome::Failed,
                        detail: None, created_folder_path: None, created_archive_path: None,
                        move_receipt: None, replace_receipt: None, copy_undo: None,
                    }],
                    retry: RetryPlan::RequiresReinspection,
                    change_hints: Vec::new(),
                },
            }).unwrap();
            owner.runtime.mark_operation_worker_returned(token);
            let key = owner.runtime.journal_attempt(token).unwrap().key;
            let review = DirectoryMoveReview {
                attempt: token, journal_key: key.clone(), source: "source".into(), destination: "destination".into(),
                children_total: 0, children_dispositions_recorded: 0, pending_child: false,
                root_disposition_recorded: false, pending_root: false, cleanup_complete: false,
                source_root_identity: Some("expected-source".into()),
                destination_root_identity: Some("expected-destination".into()),
                source_root_observation: State::Indeterminate, destination_root_observation: State::Indeterminate,
                replace: None, stage_root_observation: State::Indeterminate, backup_root_observation: State::Indeterminate,
            };
            let probe = DirectoryMoveRootProbe {
                attempt: token, journal_key: key.clone(), source: review.source.clone(), destination: review.destination.clone(),
                expected_source_identity: review.source_root_identity.clone(), expected_destination_identity: review.destination_root_identity.clone(),
                stage: None, backup: None, expected_stage_identity: None, expected_backup_identity: None,
            };
            (owner, token, key, review, probe)
        };

        let (mut owner, token, key, review, probe) = make_owner();
        owner.directory_move_reviews.insert(token, (7, DirectoryMoveReviewState::Pending));
        owner.directory_move_review_probes.insert(token, (7, key.clone(), review, probe));
        assert_eq!(owner.directory_move_root_probe(token, 7, &key).unwrap().journal_key, key);
        let observations = |source, destination| DirectoryMoveRootObservations { source, destination, stage: State::Indeterminate, backup: State::Indeterminate };
        assert!(!owner.finish_directory_move_root_probe(token, 6, &key, observations(State::Matches, State::Absent)));
        assert!(!owner.finish_directory_move_root_probe(token, 7, "wrong-key", observations(State::Matches, State::Absent)));
        assert!(owner.finish_directory_move_root_probe(token, 7, &key, DirectoryMoveRootObservations {
            source: State::Matches, destination: State::Changed, stage: State::Absent, backup: State::Matches,
        }));
        assert!(matches!(owner.directory_move_review(token), Some((7, DirectoryMoveReviewState::Available(review)))
            if review.source_root_observation == State::Matches && review.destination_root_observation == State::Changed
                && review.stage_root_observation == State::Indeterminate && review.backup_root_observation == State::Indeterminate));
        assert!(!owner.finish_directory_move_root_probe(token, 7, &key, observations(State::Absent, State::Absent)));

        let (mut reopened, token, key, review, probe) = make_owner();
        reopened.directory_move_reviews.insert(token, (3, DirectoryMoveReviewState::Pending));
        reopened.directory_move_review_probes.insert(token, (3, key.clone(), review, probe));
        reopened.cancel_directory_move_review(token);
        assert!(!reopened.finish_directory_move_root_probe(token, 3, &key, observations(State::Matches, State::Matches)));
        assert!(reopened.directory_move_review(token).is_none());
        reopened.directory_move_reviews.insert(token, (4, DirectoryMoveReviewState::Pending));
        assert!(!reopened.finish_directory_move_root_probe(token, 3, &key, observations(State::Matches, State::Matches)));
        assert_eq!(reopened.directory_move_review(token).unwrap().0, 4);
        let fresh_owner = OperationApplicationOwner::for_test();
        assert!(fresh_owner.runtime.jobs().is_empty());
        assert!(fresh_owner.directory_move_review(token).is_none());

        let (mut missing_identity_owner, missing_token, missing_key, mut missing_review, mut missing_probe) = make_owner();
        missing_review.source_root_identity = None;
        missing_probe.expected_source_identity = None;
        missing_identity_owner.directory_move_reviews.insert(missing_token, (11, DirectoryMoveReviewState::Pending));
        missing_identity_owner.directory_move_review_probes.insert(missing_token, (11, missing_key.clone(), missing_review, missing_probe));
        assert!(missing_identity_owner.finish_directory_move_root_probe(missing_token, 11, &missing_key, observations(State::Matches, State::Absent)));
        assert!(matches!(missing_identity_owner.directory_move_review(missing_token), Some((11, DirectoryMoveReviewState::Available(review)))
            if review.source_root_observation == State::Indeterminate));
    }

    #[test]
    fn directory_move_review_without_writer_is_explicitly_unavailable() {
        let mut owner = OperationApplicationOwner::for_test();
        let mut request = copy_request("C:/source/dir");
        request.kind = super::super::model::OperationKind::Move;
        let token = owner.runtime.submit(request).unwrap();
        owner.runtime.start_next().unwrap();
        owner.runtime.commit_fact(super::super::executor::ExecutorFact::Terminal {
            token,
            report: super::super::model::TerminalReport {
                outcome: super::super::model::TerminalOutcome::Partial,
                final_counters: super::super::model::ProgressCounters::default(),
                receipts: vec![super::super::model::ItemReceipt {
                    item: super::super::model::ItemId(0),
                    outcome: super::super::model::ItemOutcome::Failed,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }],
                retry: super::super::model::RetryPlan::RequiresReinspection,
                change_hints: Vec::new(),
            },
        }).unwrap();

        assert!(owner.begin_directory_move_review(token).is_none());
        assert!(matches!(
            owner.directory_move_review(token),
            Some((_, super::super::model::DirectoryMoveReviewState::Unavailable))
        ));
    }

    #[test]
    fn directory_move_review_rejects_multi_source_single_ledger() {
        use super::super::{
            executor::ExecutorFact,
            journal::{JournalDirectoryMove, JournalDirectoryMoveReplace, JournalPath},
            model::{ItemId, ItemOutcome, ItemReceipt, ProgressCounters, RetryPlan, TerminalOutcome, TerminalReport},
        };

        let mut owner = OperationApplicationOwner::for_test();
        let mut request = copy_request("C:/source/dir");
        request.kind = super::super::model::OperationKind::Move;
        request.sources.push("C:/source/second".into());
        request.destination = Some("E:/target".into());
        let token = owner.runtime.submit(request).unwrap();
        owner.runtime.start_next().unwrap();
        owner.runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Partial,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(0), outcome: ItemOutcome::Failed, detail: None,
                    created_folder_path: None, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None,
                }],
                retry: RetryPlan::RequiresReinspection,
                change_hints: Vec::new(),
            },
        }).unwrap();
        let attempt = owner.runtime.journal_attempt(token).unwrap();
        owner.directory_move_reviews.insert(token, (1, super::super::model::DirectoryMoveReviewState::Pending));
        let source = attempt.sources[0].clone();
        let destination = attempt.destination.as_ref().unwrap().to_path().unwrap().join(source.to_path().unwrap().file_name().unwrap());
        let durable = JournalAttempt {
            directory_moves: vec![JournalDirectoryMove {
                item: 0,
                source,
                destination: JournalPath::from_path(&destination),
                children: Vec::new(),
                pending: None,
                root: None,
                cleanup_complete: false,
                replace: None::<JournalDirectoryMoveReplace>,
            }],
            ..attempt.clone()
        };

        assert!(owner.commit_directory_move_review(token, 1, &attempt.key, Ok(Some(durable))));
        assert!(matches!(
            owner.directory_move_review(token),
            Some((_, super::super::model::DirectoryMoveReviewState::Unavailable))
        ));
    }

    #[test]
    fn directory_move_review_projects_exact_singleton_replace_and_four_roots() {
        use super::super::{
            executor::ExecutorFact,
            journal::{DirectoryMoveReplacePhase, JournalAttempt, JournalDirectoryMove, JournalDirectoryMoveReplace, JournalDirectoryRoot, JournalPath},
            model::{ItemId, ItemOutcome, ItemReceipt, ProgressCounters, RetryPlan, TerminalOutcome, TerminalReport},
        };
        use super::super::model::{CopyInspectionState as State, DirectoryMoveReviewState};

        let mut owner = OperationApplicationOwner::for_test();
        let mut request = copy_request("C:/source/dir");
        request.kind = super::super::model::OperationKind::Move;
        request.destination = Some("E:/target".into());
        let token = owner.runtime.submit(request).unwrap();
        owner.runtime.start_next().unwrap();
        owner.runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Partial,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt { item: ItemId(0), outcome: ItemOutcome::Failed,
                    detail: None, created_folder_path: None, created_archive_path: None,
                    move_receipt: None, replace_receipt: None, copy_undo: None }],
                retry: RetryPlan::RequiresReinspection,
                change_hints: Vec::new(),
            },
        }).unwrap();
        owner.runtime.mark_operation_worker_returned(token);
        let attempt = owner.runtime.journal_attempt(token).unwrap();
        let source = attempt.sources[0].clone();
        let destination = JournalPath::from_path(std::path::Path::new("E:/target/dir"));
        let stage = JournalPath::from_path(std::path::Path::new("E:/target/.stage"));
        let backup = JournalPath::from_path(std::path::Path::new("E:/target/.backup"));
        let backup_children = vec![
            super::super::journal::JournalDirectoryMoveReplaceChild {
                ordinal: 0, node_kind: super::super::journal::JournalDirectoryNodeKind::File,
                relative_components: vec![vec![b'a' as u16]], identity: "backup-child-0".into(),
                content_fact: "file:1:1".into(), disposition_recorded: true,
            },
            super::super::journal::JournalDirectoryMoveReplaceChild {
                ordinal: 1, node_kind: super::super::journal::JournalDirectoryNodeKind::File,
                relative_components: vec![vec![b'b' as u16]], identity: "backup-child-1".into(),
                content_fact: "file:1:1".into(), disposition_recorded: false,
            },
        ];
        let replace = JournalDirectoryMoveReplace {
            stage: stage.clone(), backup: backup.clone(), stage_identity: "incoming".into(),
            approved_target_identity: "displaced".into(), destination_parent_identity: "parent".into(),
            backup_root_identity: "displaced".into(), backup_children: backup_children.clone(),
            backup_pending: Some(super::super::journal::JournalDirectoryMoveReplaceChildIntent {
                ordinal: 1, node_kind: super::super::journal::JournalDirectoryNodeKind::File,
                relative_components: vec![vec![b'b' as u16]], identity: "backup-child-1".into(),
                content_fact: "file:1:1".into(),
            }),
            backup_root_disposition_recorded: false, backup_cleanup_complete: false,
            phase: DirectoryMoveReplacePhase::BackupCleanup,
        };
        let durable = JournalAttempt {
            directory_moves: vec![JournalDirectoryMove {
                item: 0, source: source.clone(), destination, children: Vec::new(), pending: None,
                root: Some(JournalDirectoryRoot { source_identity: "source-root".into(),
                    destination_identity: "old-destination".into(), pending: None, disposition_recorded: false }),
                cleanup_complete: true, replace: Some(replace),
            }],
            ..attempt.clone()
        };
        owner.directory_move_reviews.insert(token, (1, DirectoryMoveReviewState::Pending));
        assert!(owner.commit_directory_move_review(token, 1, &attempt.key, Ok(Some(durable.clone()))));
        let probe = owner.directory_move_root_probe(token, 1, &attempt.key).unwrap();
        assert_eq!(probe.source, std::path::PathBuf::from("C:/source/dir"));
        assert_eq!(probe.destination, std::path::PathBuf::from("E:/target/dir"));
        assert_eq!(probe.stage, Some(std::path::PathBuf::from("E:/target/.stage")));
        assert_eq!(probe.backup, Some(std::path::PathBuf::from("E:/target/.backup")));
        assert_eq!(probe.expected_destination_identity.as_deref(), Some("incoming"));
        assert_eq!(probe.expected_stage_identity.as_deref(), Some("incoming"));
        assert_eq!(probe.expected_backup_identity.as_deref(), Some("displaced"));
        assert!(owner.finish_directory_move_root_probe(token, 1, &attempt.key, super::super::model::DirectoryMoveRootObservations {
            source: State::Matches, stage: State::Matches, destination: State::Matches, backup: State::Changed,
        }));
        assert!(matches!(owner.directory_move_review(token), Some((1, DirectoryMoveReviewState::Available(review)))
            if review.replace.as_ref().is_some_and(|facts| facts.phase == DirectoryMoveReplacePhase::BackupCleanup
                && !facts.backup_root_disposition_recorded && !facts.backup_cleanup_complete
                && facts.backup_children_total == 2 && facts.backup_children_dispositions_recorded == 1
                && facts.pending_backup_intent)
                && review.source_root_observation == State::Matches
                && review.stage_root_observation == State::Matches
                && review.destination_root_observation == State::Matches
                && review.backup_root_observation == State::Changed));

        owner.directory_move_reviews.insert(token, (2, DirectoryMoveReviewState::Pending));
        let mut missing_identity = durable.clone();
        missing_identity.directory_moves[0].replace.as_mut().unwrap().stage_identity.clear();
        missing_identity.directory_moves[0].replace.as_mut().unwrap().approved_target_identity.clear();
        assert!(owner.commit_directory_move_review(token, 2, &attempt.key, Ok(Some(missing_identity))));
        let missing_probe = owner.directory_move_root_probe(token, 2, &attempt.key).unwrap();
        assert_eq!(missing_probe.expected_destination_identity, None);
        assert_eq!(missing_probe.expected_stage_identity, None);
        assert_eq!(missing_probe.expected_backup_identity, None);
        assert!(owner.finish_directory_move_root_probe(token, 2, &attempt.key, super::super::model::DirectoryMoveRootObservations {
            source: State::Matches, stage: State::Matches, destination: State::Matches, backup: State::Matches,
        }));
        assert!(matches!(owner.directory_move_review(token), Some((2, DirectoryMoveReviewState::Available(review)))
            if review.stage_root_observation == State::Indeterminate
                && review.destination_root_observation == State::Indeterminate
                && review.backup_root_observation == State::Indeterminate));

        owner.directory_move_reviews.insert(token, (3, DirectoryMoveReviewState::Pending));
        let mut multiple = durable.clone();
        multiple.directory_moves.push(multiple.directory_moves[0].clone());
        assert!(owner.commit_directory_move_review(token, 3, &attempt.key, Ok(Some(multiple))));
        assert!(matches!(owner.directory_move_review(token), Some((3, DirectoryMoveReviewState::Unavailable))));

        owner.directory_move_reviews.insert(token, (4, DirectoryMoveReviewState::Pending));
        let mut mixed = durable;
        mixed.kind = super::super::journal::JournalKind::Copy;
        assert!(owner.commit_directory_move_review(token, 4, &attempt.key, Ok(Some(mixed))));
        assert!(matches!(owner.directory_move_review(token), Some((4, DirectoryMoveReviewState::Unavailable))));
    }

    fn copy_request(path: &str) -> super::super::model::OperationRequest {
        use super::super::model::{OperationKind, OperationRequest};
        OperationRequest {
            kind: OperationKind::Copy,
            virtual_source: None,
            sources: vec![path.into()],
            destination: Some("target".into()),
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
            batch_rename_plan: None,
            origin: None,
        }
    }

    fn copy_failure_fact(
        token: super::super::model::AttemptToken,
    ) -> super::super::executor::ExecutorFact {
        use super::super::model::{
            CopyFailureProvenance, CopyPublicationState, CopySourceSignatureSemantics, ItemId,
            SourceCleanupLeafSignature,
        };
        super::super::executor::ExecutorFact::CopyFailureProvenance {
            token,
            provenance: CopyFailureProvenance {
                item: ItemId(0),
                source: "source.txt".into(),
                destination: "target/source.txt".into(),
                publication: CopyPublicationState::Unknown,
                source_identity: Some(
                    crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                        "app-copy-source".into(),
                    ),
                ),
                source_signature: Some(SourceCleanupLeafSignature {
                    number_of_links: 1,
                    change_time: 1,
                }),
                source_signature_semantics:
                    CopySourceSignatureSemantics::ChangeTimeAndLinkCountFromOpenHandle,
                destination_parent_identity: None,
                stage: None,
            },
        }
    }

    fn copy_terminal() -> super::super::model::TerminalReport {
        use super::super::model::{
            FsChangeHint, ItemId, ItemOutcome, ItemReceipt, ProgressCounters, RetryPlan,
            TerminalOutcome, TerminalReport,
        };
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
                directory: "target".into(),
            }],
        }
    }

    #[test]
    fn durable_recovery_discard_removes_only_exact_key_and_is_idempotent() {
        let mut owner = OperationApplicationOwner::for_test();
        owner.recovery_records = vec![test_attempt("K"), test_attempt("L")];

        assert!(owner.discard_recovery_record("K"));
        assert_eq!(owner.recovery_records.len(), 1);
        assert_eq!(owner.recovery_records[0].key, "L");
        assert!(!owner.discard_recovery_record("K"));
        assert_eq!(owner.recovery_records[0].key, "L");
    }

    #[test]
    fn recovery_copy_activation_uses_one_fresh_ticket_and_preserves_history() {
        use super::super::journal::JournalPath;
        use std::path::PathBuf;

        let source = PathBuf::from("C:/source/current.txt");
        let destination = PathBuf::from("C:/destination");
        let mut attempt = test_attempt("interrupted-copy");
        attempt.sources = vec![JournalPath::from_path(&source)];
        attempt.destination = Some(JournalPath::from_path(&destination));
        let original_record = serde_json::to_vec(&attempt).unwrap();

        let journal_dir = tempfile::tempdir().unwrap();
        let writer = super::super::journal::JournalWriter::start(
            super::super::journal::JournalStore::open(
                journal_dir.path().join("operation-journal.json"),
            )
            .unwrap(),
        );
        let mut owner = OperationApplicationOwner::for_test();
        owner.journal = Some(writer.clone());
        owner.recovery_records = vec![attempt.clone()];
        assert!(owner.recovery_copy_action_available(&attempt));

        let (stale_ticket, _, _) = owner
            .begin_recovery_copy_activation(&attempt)
            .expect("first activation reserves the recovery record");
        assert!(!owner.recovery_copy_action_available(&attempt));
        assert!(owner.begin_recovery_copy_activation(&attempt).is_none());

        owner.cancel_recovery_copy_activation(&stale_ticket);
        let (current_ticket, sources, current_destination) = owner
            .begin_recovery_copy_activation(&attempt)
            .expect("cancelled activation can be retried");
        assert_eq!(sources, vec![source.clone()]);
        assert_eq!(current_destination, destination);
        owner.cancel_recovery_copy_activation(&stale_ticket);
        assert!(
            !owner.recovery_copy_action_available(&attempt),
            "stale cancellation cannot release a newer ticket"
        );

        let token = owner
            .submit_recovery_copy_activation(
                &current_ticket,
                vec![crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 10)],
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 20),
            )
            .expect("fresh guarded Copy is admitted through Runtime");
        assert_eq!(token.job_id().value(), 1);
        let queued = owner.runtime.journal_attempt(token).unwrap();
        assert_eq!(queued.state, super::super::journal::JournalState::Queued);
        writer
            .upsert_with_reply(queued.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        assert_eq!(
            writer
                .read_attempt(queued.key.clone())
                .recv_blocking()
                .unwrap()
                .unwrap(),
            Some(queued.clone()),
            "the queued admission record is durable before Runtime releases the attempt"
        );
        assert!(owner.resolve_recovery_copy_admission(
            &current_ticket,
            token,
            Some(&queued),
            true,
        ));
        owner.runtime.start_next().expect("accepted Copy starts");
        owner
            .runtime
            .commit_fact(super::super::executor::ExecutorFact::Terminal {
                token,
                report: copy_terminal(),
            })
            .expect("successful Copy terminal is committed");
        assert_eq!(
            serde_json::to_vec(&owner.recovery_records[0]).unwrap(),
            original_record
        );
        assert!(
            !owner.recovery_copy_action_available(&attempt),
            "accepted activation stays one-shot"
        );
    }

    #[test]
    fn recovery_copy_queued_ack_before_worker_start_recovers_without_replay() {
        use super::super::journal::{JournalPath, JournalState, JournalStore, RecoveryItem};
        use std::path::PathBuf;

        let source = PathBuf::from("C:/source/current.txt");
        let destination = PathBuf::from("C:/destination");
        let mut historical = test_attempt("interrupted-copy-before-queued-ack");
        historical.sources = vec![JournalPath::from_path(&source)];
        historical.destination = Some(JournalPath::from_path(&destination));
        let journal_dir = tempfile::tempdir().unwrap();
        let journal_path = journal_dir.path().join("operation-journal.json");
        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        store.upsert(historical.clone()).unwrap();
        let mut owner = OperationApplicationOwner::for_test_with_journal_store_and_workers(store);
        let recovery = owner
            .recovery_records
            .iter()
            .find(|record| record.key == historical.key)
            .expect("the interrupted source Copy is visible for an explicit retry")
            .clone();
        let (ticket, _, _) = owner
            .begin_recovery_copy_activation(&recovery)
            .expect("Recovery Copy reserves the exact historical record");
        let token = owner
            .submit_recovery_copy_activation(
                &ticket,
                vec![crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                    1, 10,
                )],
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 20),
            )
            .expect("fresh Copy is held until durable admission");
        let queued = owner.runtime.journal_attempt(token).unwrap();
        assert_eq!(queued.state, JournalState::Queued);
        owner
            .journal
            .as_ref()
            .expect("test owner uses its serial JournalWriter")
            .upsert_with_reply(queued.clone())
            .recv_blocking()
            .unwrap()
            .unwrap();
        assert_eq!(
            owner
                .journal
                .as_ref()
                .unwrap()
                .read_attempt(queued.key.clone())
                .recv_blocking()
                .unwrap()
                .unwrap(),
            Some(queued.clone()),
            "the exact Queued attempt is durable before worker release"
        );
        assert!(owner.runtime.start_next().is_none());
        assert!(owner.claim_next_worker().is_none());
        let durable_bytes = std::fs::read(&journal_path).unwrap();
        drop(owner);

        let mut expected_historical = historical;
        expected_historical.state = JournalState::RecoveryRequired;
        let mut expected_queued = queued;
        expected_queued.state = JournalState::RecoveryRequired;
        for _ in 0..2 {
            let store = JournalStore::open(journal_path.clone()).unwrap();
            let recoveries = store.recoveries();
            let [
                RecoveryItem::RecoveryRequired(recovered_historical),
                RecoveryItem::RecoveryRequired(recovered_queued),
            ] = recoveries.as_slice()
            else {
                panic!(
                    "historical and unstarted Queued attempts recover as inspection-only records"
                );
            };
            assert_eq!(recovered_historical, &expected_historical);
            assert_eq!(recovered_queued, &expected_queued);
            let mut owner =
                OperationApplicationOwner::for_test_with_journal_store_and_workers(store);
            assert_eq!(
                owner.recovery_records,
                [expected_historical.clone(), expected_queued.clone()]
            );
            assert!(
                owner.runtime.jobs().is_empty(),
                "fresh owner does not replay Recovery Copy"
            );
            assert!(owner.claim_next_worker().is_none());
            assert_eq!(std::fs::read(&journal_path).unwrap(), durable_bytes);
            drop(owner);
        }
    }

    #[test]
    fn recovery_copy_durable_admission_failure_removes_only_the_held_attempt() {
        use super::super::journal::JournalPath;
        use std::path::PathBuf;

        let source = PathBuf::from("C:/source/current.txt");
        let destination = PathBuf::from("C:/destination");
        let mut historical = test_attempt("interrupted-copy-admission-failure");
        historical.sources = vec![JournalPath::from_path(&source)];
        historical.destination = Some(JournalPath::from_path(&destination));
        let original = serde_json::to_vec(&historical).unwrap();

        let mut owner = OperationApplicationOwner::for_test();
        owner.worker_claims_reserved_for_test = false;
        owner.recovery_records = vec![historical.clone()];
        let (ticket, sources, target) = owner
            .begin_recovery_copy_activation(&historical)
            .expect("recovery action starts one activation");
        let token = owner
            .submit_recovery_copy_activation(
                &ticket,
                vec![crate::filesystem_identity::FilesystemIdentity::from_windows_parts(3, 10)],
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(3, 20),
            )
            .expect("fresh request is held pending durable admission");
        assert!(!owner.discard_recovery_record(&historical.key));
        assert_eq!(sources, vec![source]);
        assert_eq!(target, destination);
        assert!(
            owner.claim_next_worker().is_none(),
            "unacknowledged admission cannot start"
        );
        let queued = owner.runtime.journal_attempt(token).unwrap();
        assert_eq!(queued.state, super::super::journal::JournalState::Queued);

        let unrelated = owner.runtime.submit_held(copy_request("unrelated.txt")).unwrap();
        assert!(!owner.resolve_recovery_copy_admission(&ticket, unrelated, Some(&queued), false));
        assert!(
            owner.runtime.job(token).is_some(),
            "a mismatched token cannot reject the admitted Copy"
        );
        assert!(
            owner.runtime.job(unrelated).is_some(),
            "a mismatched token cannot delete another held job"
        );

        assert!(!owner.resolve_recovery_copy_admission(&ticket, token, Some(&queued), false));
        assert!(
            owner.runtime.job(token).is_none(),
            "failed admission leaves no runnable job"
        );
        assert!(
            owner.runtime.job(unrelated).is_some(),
            "unrelated held work remains intact"
        );
        assert!(owner.recovery_copy_action_available(&historical));
        assert_eq!(serde_json::to_vec(&owner.recovery_records[0]).unwrap(), original);
        assert!(owner.runtime.reject_held(unrelated));
    }

    #[test]
    fn recovery_record_discard_reservation_blocks_new_copy_until_durable_reply() {
        use super::super::journal::JournalPath;
        use std::path::PathBuf;

        let mut attempt = test_attempt("interrupted-copy-discard-race");
        attempt.sources = vec![JournalPath::from_path(&PathBuf::from("C:/source.txt"))];
        attempt.destination = Some(JournalPath::from_path(&PathBuf::from("C:/destination")));
        let mut owner = OperationApplicationOwner::for_test();
        owner.recovery_records = vec![attempt.clone()];

        assert!(owner.begin_recovery_record_discard(&attempt.key));
        assert!(!owner.begin_recovery_record_discard(&attempt.key));
        assert!(!owner.recovery_copy_action_available(&attempt));
        assert!(owner.begin_recovery_copy_activation(&attempt).is_none());
        assert!(!owner.finish_recovery_record_discard(&attempt.key, false));
        assert!(owner.recovery_copy_action_available(&attempt));

        assert!(owner.begin_recovery_record_discard(&attempt.key));
        assert!(owner.finish_recovery_record_discard(&attempt.key, true));
        assert!(owner.recovery_records.is_empty());
        assert!(!owner.recovery_copy_action_available(&attempt));
    }

    #[test]
    fn copy_inspection_return_releases_serial_lane_for_complete_stale_and_cancelled_results() {
        use super::super::model::{
            CopyFailureInspectionCandidate, CopyFailureInspectionResult, CopyInspectionState,
            ExecutorCommand,
        };

        for completion in ["complete", "stale", "cancelled"] {
            let mut owner = OperationApplicationOwner::for_test();
            owner.live_shells.set(1);
            owner.worker_claims_reserved_for_test = false;
            let attempt = owner.runtime.submit(copy_request("source.txt")).unwrap();
            let queued = owner.runtime.submit(copy_request("queued.txt")).unwrap();
            let worker = owner.claim_next_worker().expect("first worker claims lane");
            assert!(matches!(worker.command, ExecutorCommand::Prepare { token, .. } if token == attempt));
            assert!(owner.runtime.commit_fact(copy_failure_fact(attempt)).is_none());
            owner
                .runtime
                .commit_fact(super::super::executor::ExecutorFact::Terminal {
                    token: attempt,
                    report: copy_terminal(),
                })
                .unwrap();

            let (
                next,
                move_inspection,
                cross_volume_move_inspection,
                published_move_inspection,
                inspection,
            ) = owner.worker_returned_and_claim_next(attempt);
            assert!(next.is_none());
            assert!(move_inspection.is_none());
            assert!(cross_volume_move_inspection.is_none());
            assert!(published_move_inspection.is_none());
            let inspection = inspection.expect("copy inspection reserves the lane");
            assert!(owner.worker_claimed_for_test());
            assert!(owner.claim_next_worker().is_none(), "queued operation waits");

            let returned_result = match completion {
                "stale" => {
                    let _newer = owner
                        .runtime
                        .begin_copy_failure_inspection(attempt)
                        .expect("newer ticket supersedes the in-flight one");
                    inspection
                }
                "cancelled" => {
                    assert!(owner
                        .runtime
                        .cancel_copy_failure_inspection(inspection.ticket));
                    inspection
                }
                _ => CopyFailureInspectionResult {
                    ticket: inspection.ticket,
                    attempt: inspection.attempt,
                    candidates: inspection
                        .candidates
                        .iter()
                        .map(|candidate| CopyFailureInspectionCandidate {
                            provenance: candidate.provenance.clone(),
                            source: CopyInspectionState::Matches,
                            destination_parent: CopyInspectionState::Matches,
                            destination: CopyInspectionState::Absent,
                            stage: CopyInspectionState::Indeterminate,
                            stage_content: CopyInspectionState::Indeterminate,
                        })
                        .collect(),
                    directory_candidates: inspection.directory_candidates.clone(),
                },
            };
            let next = owner.copy_inspection_worker_returned_and_claim_next(returned_result);
            let next = next.expect("inspection return releases lane to queued operation");
            assert!(matches!(next.command, ExecutorCommand::Prepare { token, .. } if token == queued));
            assert!(owner.worker_claimed_for_test());
        }
    }

    #[test]
    fn mixed_cross_volume_move_inspections_keep_application_lane_until_both_commit() {
        use super::super::{
            executor::ExecutorFact,
            model::{
                CrossVolumeMoveInspectionCandidate, CrossVolumeMoveNoEffectProof, ExecutorCommand,
                FsChangeHint, ItemId, ItemOutcome, ItemReceipt, MoveReceipt, OperationKind,
                ProgressCounters, PublishedMoveInspectionCandidate,
                PublishedMoveSourceRetainedProof, RetryPlan, SourceCleanupLeafSignature,
                TerminalOutcome, TerminalReport,
            },
        };
        use std::path::PathBuf;

        let mut owner = OperationApplicationOwner::for_test();
        owner.live_shells.set(1);
        owner.worker_claims_reserved_for_test = false;

        let source_failed = PathBuf::from("C:/source/fails-before-publish.bin");
        let source_published = PathBuf::from("C:/source/published-source-retained.bin");
        let destination_root = PathBuf::from("E:/destination");
        let destination_failed = destination_root.join("fails-before-publish.bin");
        let destination_published = destination_root.join("published-source-retained.bin");
        let mut request = copy_request(source_failed.to_str().unwrap());
        request.kind = OperationKind::Move;
        request.sources = vec![source_failed.clone(), source_published.clone()];
        request.destination = Some(destination_root.clone());
        let attempt = owner.runtime.submit(request).unwrap();
        let queued = owner.runtime.submit(copy_request("queued.txt")).unwrap();

        let worker = owner.claim_next_worker().expect("A claims the serial lane");
        assert!(
            matches!(worker.command, ExecutorCommand::Prepare { token, .. } if token == attempt)
        );
        let source_signature = SourceCleanupLeafSignature {
            number_of_links: 1,
            change_time: 101,
        };
        let failed_proof = CrossVolumeMoveNoEffectProof {
            item: ItemId(0),
            source: source_failed,
            destination: destination_failed,
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                10, 1,
            ),
            source_signature,
            destination_parent_identity:
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(20, 1),
        };
        let published_proof = PublishedMoveSourceRetainedProof {
            item: ItemId(1),
            source: source_published,
            destination: destination_published,
            source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                10, 2,
            ),
            source_signature: SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 102,
            },
            destination_parent_identity:
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(20, 1),
            destination_identity:
                crate::filesystem_identity::FilesystemIdentity::from_windows_parts(20, 2),
            destination_signature: SourceCleanupLeafSignature {
                number_of_links: 1,
                change_time: 202,
            },
        };
        assert!(
            owner
                .runtime
                .commit_fact(ExecutorFact::CrossVolumeMoveNoEffectProof {
                    token: attempt,
                    proof: failed_proof.clone(),
                })
                .is_none()
        );
        assert!(
            owner
                .runtime
                .commit_fact(ExecutorFact::PublishedMoveSourceRetained {
                    token: attempt,
                    proof: published_proof.clone(),
                })
                .is_none()
        );
        owner
            .runtime
            .commit_fact(ExecutorFact::Terminal {
                token: attempt,
                report: TerminalReport {
                    outcome: TerminalOutcome::Partial,
                    final_counters: ProgressCounters {
                        processed_items: 2,
                        ..ProgressCounters::default()
                    },
                    receipts: vec![
                        ItemReceipt {
                            item: ItemId(0),
                            outcome: ItemOutcome::Failed,
                            detail: Some("pre-publication failure".into()),
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: None,
                            replace_receipt: None,
                            copy_undo: None,
                        },
                        ItemReceipt {
                            item: ItemId(1),
                            outcome: ItemOutcome::Partial,
                            detail: Some("destination published; source retained".into()),
                            created_folder_path: None,
                            created_archive_path: None,
                            move_receipt: Some(MoveReceipt {
                                source: published_proof.source.clone(),
                                destination: published_proof.destination.clone(),
                                destination_published: true,
                                source_removed: false,
                            }),
                            replace_receipt: None,
                            copy_undo: None,
                        },
                    ],
                    retry: RetryPlan::RequiresReinspection,
                    change_hints: vec![FsChangeHint {
                        directory: destination_root.clone(),
                    }],
                },
            })
            .expect("terminal commits once for A");

        let terminal_snapshot = owner
            .runtime
            .job(attempt)
            .unwrap()
            .attempts
            .last()
            .unwrap()
            .clone();
        let journal_snapshot = owner.runtime.journal_attempt(attempt).unwrap();
        let (next, move_inspection, cross_inspection, published_inspection, copy_inspection) =
            owner.worker_returned_and_claim_next(attempt);
        assert!(next.is_none());
        assert!(move_inspection.is_none());
        assert!(copy_inspection.is_none());
        let cross_inspection = cross_inspection.expect("pre-publication proof is inspected first");
        assert!(published_inspection.is_none());
        assert_eq!(cross_inspection.attempt, attempt);
        assert_eq!(cross_inspection.candidates.len(), 1);
        assert_eq!(cross_inspection.candidates[0].proof, failed_proof);
        assert!(owner.worker_claimed_for_test());
        assert!(
            owner.claim_next_worker().is_none(),
            "B stays queued during first inspection"
        );
        let cross_ticket = cross_inspection.ticket;
        let cross_result = super::super::model::CrossVolumeMoveInspectionResult {
            ticket: cross_ticket,
            attempt,
            candidates: vec![CrossVolumeMoveInspectionCandidate {
                proof: failed_proof.clone(),
                source_matches: true,
                destination_parent_matches: true,
                destination: super::super::model::GuardedMoveDestinationState::Absent,
            }],
        };
        let (next, published_inspection) =
            owner.cross_volume_move_inspection_worker_returned_and_claim_next(cross_result);
        assert!(
            next.is_none(),
            "cross-volume result commits without claiming B"
        );
        let published_inspection = published_inspection.expect("published Move inspection follows");
        assert_eq!(published_inspection.attempt, attempt);
        assert_eq!(published_inspection.candidates.len(), 1);
        assert_eq!(published_inspection.candidates[0].proof, published_proof);
        assert!(owner.worker_claimed_for_test());
        assert!(
            owner.claim_next_worker().is_none(),
            "B stays queued during second inspection"
        );
        let (history_ticket, history) = owner
            .runtime
            .latest_cross_volume_move_inspection(attempt)
            .expect("first inspection history is retained");
        assert_eq!(history_ticket, cross_ticket);
        let history = history.expect("first result committed");
        assert_eq!(history.attempt, attempt);
        assert_eq!(history.candidates[0].proof, failed_proof);
        assert_eq!(
            owner.runtime.journal_attempt(attempt).unwrap(),
            journal_snapshot
        );

        let published_ticket = published_inspection.ticket;
        let published_result = super::super::model::PublishedMoveInspectionResult {
            ticket: published_ticket,
            attempt,
            synthesized_unknown: false,
            candidates: vec![PublishedMoveInspectionCandidate {
                proof: published_proof.clone(),
                source: super::super::model::PublishedMoveInspectionState::Matches,
                destination_parent: super::super::model::PublishedMoveInspectionState::Matches,
                destination: super::super::model::PublishedMoveInspectionState::Matches,
            }],
        };
        let next = owner
            .published_move_inspection_worker_returned_and_claim_next(published_result)
            .expect("B is claimable only after the second inspection commits");
        assert!(matches!(next.command, ExecutorCommand::Prepare { token, .. } if token == queued));
        assert!(owner.worker_claimed_for_test());
        let (cross_ticket_after, cross_history) = owner
            .runtime
            .latest_cross_volume_move_inspection(attempt)
            .expect("cross-volume history remains retrievable after B starts");
        let (published_ticket_after, published_history) = owner
            .runtime
            .latest_published_move_inspection(attempt)
            .expect("published-Move history remains retrievable after B starts");
        assert_eq!(cross_ticket_after, cross_ticket);
        assert_eq!(published_ticket_after, published_ticket);
        assert_ne!(cross_ticket, published_ticket);
        assert_eq!(cross_history.unwrap().candidates[0].proof, failed_proof);
        assert_eq!(
            published_history.unwrap().candidates[0].proof,
            published_proof
        );
        let terminal_after = owner
            .runtime
            .job(attempt)
            .unwrap()
            .attempts
            .last()
            .unwrap()
            .clone();
        assert_eq!(terminal_after, terminal_snapshot);
        assert_eq!(
            terminal_after.terminal.as_ref().unwrap().retry,
            RetryPlan::RequiresReinspection
        );
        assert_eq!(
            owner.runtime.journal_attempt(attempt).unwrap(),
            journal_snapshot
        );
    }

    #[test]
    fn startup_recovery_and_writer_share_the_loaded_journal_store() {
        use super::super::journal::{JournalPath, JournalState, JournalTerminalOutcome};
        use std::{fs, path::Path};

        let root = tempfile::tempdir().unwrap();
        let journal_path = root.path().join("operations.json");
        let source = root.path().join("source-sentinel.txt");
        let destination = root.path().join("destination-sentinel.txt");
        fs::write(&source, b"source remains untouched").unwrap();
        fs::write(&destination, b"destination remains untouched").unwrap();

        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        for key in ["K", "L"] {
            let mut record = test_attempt(key);
            record.sources = vec![JournalPath::from_path(Path::new(&source))];
            record.destination = Some(JournalPath::from_path(Path::new(&destination)));
            store.upsert(record).unwrap();
        }
        let mut terminal = test_attempt("T");
        terminal.state = JournalState::Terminal;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        store.upsert(terminal).unwrap();
        let journal_before_startup = fs::read(&journal_path).unwrap();

        let mut owner = OperationApplicationOwner::from_journal_store(Some(store), None);
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["K", "L"]
        );
        assert!(owner.journal.is_some());
        assert_eq!(fs::read(&journal_path).unwrap(), journal_before_startup);
        assert_eq!(fs::read(&source).unwrap(), b"source remains untouched");
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"destination remains untouched"
        );

        let receipt = owner.journal.as_ref().unwrap().discard("K".into());
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["K", "L"]
        );
        assert!(matches!(receipt.recv_blocking().unwrap(), Ok(true)));

        let reopened = JournalStore::open(journal_path).unwrap();
        assert!(matches!(
            reopened.recoveries().as_slice(),
            [super::super::journal::RecoveryItem::RecoveryRequired(record)] if record.key == "L"
        ));
        assert_eq!(reopened.attempt_keys(), ["L", "T"]);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("operations.json")).unwrap())
                .unwrap();
        assert!(
            persisted["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| { record["key"] == "T" && record["state"] == "terminal" })
        );

        assert!(owner.discard_recovery_record("K"));
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["L"]
        );
        assert!(!owner.discard_recovery_record("K"));
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["L"]
        );
    }

    #[test]
    fn failed_durable_discard_keeps_recovery_snapshot() {
        use super::super::journal::{JournalPath, JournalState, JournalTerminalOutcome};
        use std::fs;
        use std::path::Path;

        let root = tempfile::tempdir().unwrap();
        let journal_directory = root.path().join("journal");
        fs::create_dir(&journal_directory).unwrap();
        let journal_path = journal_directory.join("operations.json");
        let source = root.path().join("source-sentinel.txt");
        let destination = root.path().join("destination-sentinel.txt");
        fs::write(&source, b"source remains untouched").unwrap();
        fs::write(&destination, b"destination remains untouched").unwrap();

        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        for key in ["K", "L"] {
            let mut record = test_attempt(key);
            record.sources = vec![JournalPath::from_path(Path::new(&source))];
            record.destination = Some(JournalPath::from_path(Path::new(&destination)));
            store.upsert(record).unwrap();
        }
        let mut terminal = test_attempt("T");
        terminal.state = JournalState::Terminal;
        terminal.terminal_outcome = Some(JournalTerminalOutcome::Succeeded);
        store.upsert(terminal).unwrap();
        let original_journal_bytes = fs::read(&journal_path).unwrap();

        let mut owner = OperationApplicationOwner::from_journal_store(Some(store), None);
        let displaced_directory = root.path().join("journal-displaced");
        fs::rename(&journal_directory, &displaced_directory).unwrap();
        // create_dir_all cannot recreate this path because it is now a regular file.
        fs::write(&journal_directory, b"block journal parent recreation").unwrap();

        let receipt = owner.journal.as_ref().unwrap().discard("K".into());
        assert!(receipt.recv_blocking().unwrap().is_err());
        assert_eq!(
            fs::read(displaced_directory.join("operations.json")).unwrap(),
            original_journal_bytes
        );
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["K", "L"]
        );

        fs::remove_file(&journal_directory).unwrap();
        fs::rename(&displaced_directory, &journal_directory).unwrap();

        let receipt = owner.journal.as_ref().unwrap().discard("K".into());
        assert!(matches!(receipt.recv_blocking().unwrap(), Ok(true)));
        let receipt = owner.journal.as_ref().unwrap().discard("K".into());
        assert!(matches!(receipt.recv_blocking().unwrap(), Ok(false)));
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["K", "L"]
        );

        assert!(owner.discard_recovery_record("K"));
        assert_eq!(
            owner
                .recovery_records
                .iter()
                .map(|record| record.key.as_str())
                .collect::<Vec<_>>(),
            ["L"]
        );
        assert!(!owner.discard_recovery_record("K"));

        let reopened = JournalStore::open(journal_path).unwrap();
        assert_eq!(reopened.attempt_keys(), ["L", "T"]);
        assert!(matches!(
            reopened.recoveries().as_slice(),
            [super::super::journal::RecoveryItem::RecoveryRequired(record)] if record.key == "L"
        ));
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("journal/operations.json")).unwrap())
                .unwrap();
        assert!(
            persisted["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| { record["key"] == "T" && record["state"] == "terminal" })
        );
        assert_eq!(fs::read(&source).unwrap(), b"source remains untouched");
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"destination remains untouched"
        );

        // Drop the owner to close the writer channel before temporary-directory cleanup.
        drop(owner);
    }

    #[test]
    fn reopened_journal_does_not_reconstruct_process_local_copy_evidence() {
        use super::super::journal::{JournalPath, JournalState};
        use std::{fs, path::Path};

        let root = tempfile::tempdir().unwrap();
        let journal_path = root.path().join("operations.json");
        let source = root.path().join("source.bin");
        let destination = root.path().join("destination.bin");
        fs::write(&source, b"source sentinel").unwrap();
        fs::write(&destination, b"destination sentinel").unwrap();

        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        let mut record = test_attempt("copy-failure-evidence");
        record.state = JournalState::RecoveryRequired;
        record.sources = vec![JournalPath::from_path(Path::new(&source))];
        record.destination = Some(JournalPath::from_path(Path::new(&destination)));
        store.upsert(record).unwrap();
        drop(store);

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert!(
            !persisted.to_string().contains("copy_failure_provenance"),
            "typed passive evidence is not part of the journal schema"
        );

        let owner = OperationApplicationOwner::from_journal_store(
            Some(JournalStore::open(journal_path).unwrap()),
            None,
        );
        assert_eq!(owner.recovery_records.len(), 1);
        assert!(owner.runtime.jobs().is_empty());
        assert_eq!(fs::read(&source).unwrap(), b"source sentinel");
        assert_eq!(fs::read(&destination).unwrap(), b"destination sentinel");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "spawned by process_crash_reopens_nonterminal_journal_as_inspection_only_recovery"]
    fn operation_application_process_crash_child() {
        use super::super::journal::{JournalPath, JournalState};
        use std::{env, fs, path::Path};

        let Ok(root) = env::var("GPUI_FILER_OPERATION_RECOVERY_CHILD_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let journal_path = root.join("operations.json");
        let source = root.join("source-sentinel.txt");
        let destination = root.join("destination-sentinel.txt");
        let owner = OperationApplicationOwner::from_journal_store(
            Some(JournalStore::open(journal_path).unwrap()),
            None,
        );
        let mut attempt = test_attempt("process-crash-running-copy");
        attempt.state = JournalState::Running;
        attempt.sources = vec![JournalPath::from_path(Path::new(&source))];
        attempt.destination = Some(JournalPath::from_path(Path::new(&destination)));

        let durable_ack = owner
            .journal
            .as_ref()
            .expect("child owner must have its journal writer")
            .upsert_with_reply(attempt);
        durable_ack
            .recv_blocking()
            .expect("journal writer must reply")
            .expect("nonterminal operation record must be durably accepted");

        // The parent observes readiness only after the writer's durable ACK.
        fs::write(root.join("durable-ready"), b"acknowledged").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    #[cfg(windows)]
    #[test]
    fn process_crash_reopens_nonterminal_journal_as_inspection_only_recovery() {
        use super::super::journal::{JournalAttempt, JournalPath, JournalState};
        use std::{
            fs,
            path::Path,
            process::{Child, Command, Stdio},
            thread,
            time::{Duration, Instant},
        };

        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }

        let root = tempfile::tempdir().unwrap();
        let journal_path = root.path().join("operations.json");
        let source = root.path().join("source-sentinel.txt");
        let destination = root.path().join("destination-sentinel.txt");
        fs::write(&source, b"source survives process crash").unwrap();
        fs::write(&destination, b"destination survives process crash").unwrap();

        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("operations::application::tests::operation_application_process_crash_child")
                .arg("--nocapture")
                .arg("--ignored")
                .env("GPUI_FILER_OPERATION_RECOVERY_CHILD_ROOT", root.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn journal writer child"),
        );
        let ready = root.path().join("durable-ready");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if ready.exists() {
                break;
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("journal child exited before durable-ready: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "journal child durable ACK timed out"
            );
            thread::sleep(Duration::from_millis(20));
        }

        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child must still be alive after ACK"
        );
        child.0.kill().expect("forcibly terminate journal child");
        let status = child.0.wait().expect("reap forcibly terminated child");
        assert!(
            !status.success(),
            "forced process termination must not be a clean exit"
        );

        let durable_bytes = fs::read(&journal_path).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&durable_bytes).unwrap();
        let persisted: JournalAttempt =
            serde_json::from_value(document["attempts"][0].clone()).unwrap();
        let mut expected = test_attempt("process-crash-running-copy");
        expected.state = JournalState::Running;
        expected.sources = vec![JournalPath::from_path(Path::new(&source))];
        expected.destination = Some(JournalPath::from_path(Path::new(&destination)));
        assert_eq!(
            persisted, expected,
            "the exact acknowledged record must survive the crash"
        );

        let store = JournalStore::open(journal_path).unwrap();
        let owner = OperationApplicationOwner::from_journal_store(Some(store), None);
        let mut expected_recovery = expected;
        expected_recovery.state = JournalState::RecoveryRequired;
        assert_eq!(owner.recovery_records, [expected_recovery]);
        assert_eq!(
            owner.runtime.jobs(),
            [],
            "startup must not replay the operation"
        );
        assert_eq!(
            fs::read(&root.path().join("operations.json")).unwrap(),
            durable_bytes
        );
        assert_eq!(fs::read(&source).unwrap(), b"source survives process crash");
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"destination survives process crash"
        );
        drop(owner);
    }

    fn test_attempt(key: &str) -> JournalAttempt {
        // Deserialize defaults keep this fixture aligned with the journal's evolving schema.
        serde_json::from_value(serde_json::json!({
            "key": key,
            "kind": "copy",
            "state": "recovery-required",
            "sources": [],
            "destination": null,
            "receipts_recorded": false
        }))
        .expect("minimal recovery record")
    }

    fn reopened_directory_move_attempt(key: &str) -> JournalAttempt {
        use super::super::journal::{JournalDirectoryMove, JournalDirectoryRoot, JournalPath};
        let source = std::path::PathBuf::from(r"C:\source\folder");
        let parent = std::path::PathBuf::from(r"C:\destination");
        let destination = parent.join("folder");
        let mut attempt = test_attempt(key);
        attempt.kind = super::super::journal::JournalKind::Move;
        attempt.state = super::super::journal::JournalState::RecoveryRequired;
        attempt.sources = vec![JournalPath::from_path(&source)];
        attempt.destination = Some(JournalPath::from_path(&parent));
        attempt.directory_moves = vec![JournalDirectoryMove {
            item: 0, source: JournalPath::from_path(&source), destination: JournalPath::from_path(&destination),
            children: Vec::new(), pending: None,
            root: Some(JournalDirectoryRoot { source_identity: "source-id".into(), destination_identity: "destination-id".into(), pending: None, disposition_recorded: false }),
            cleanup_complete: false, replace: None,
        }];
        attempt
    }

    #[test]
    fn directory_copy_undo_settlement_uses_fresh_ticket_and_custody_gate() {
        use super::super::{
            executor::{DirectoryCopyUndoCustody, ExecutorFact},
            model::{
                DirectoryCopyUndoReceipt, ItemId, ItemOutcome, ItemReceipt, ProgressCounters,
                RetryPlan, SourceCleanupParentIdentity, TerminalOutcome, TerminalReport,
            },
        };
        let receipt = DirectoryCopyUndoReceipt {
            destination: "target/source".into(),
            destination_parent_route: vec![SourceCleanupParentIdentity {
                path: "target".into(),
                identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            }],
            root_identity: "1:0000000000000003".into(),
            entries: Vec::new(),
        };
        let mut owner = OperationApplicationOwner::for_test();
        let token = owner.runtime.submit(copy_request("source")).unwrap();
        owner.runtime.start_next().unwrap();
        owner.runtime.commit_fact(ExecutorFact::DirectoryCopyUndoCandidate {
            token, item: ItemId(0), receipt: receipt.clone(),
        });
        owner.runtime.commit_fact(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: ProgressCounters::default(),
                receipts: vec![ItemReceipt {
                    item: ItemId(0), outcome: ItemOutcome::Completed, detail: None,
                    created_folder_path: None, created_archive_path: None, move_receipt: None,
                    replace_receipt: None, copy_undo: None,
                }],
                retry: RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            },
        }).unwrap();
        let (reservation, admitted) = owner.reserve_undo().unwrap();
        assert!(matches!(admitted, UndoEntry::DirectoryCopy(actual) if actual == receipt));
        let stale_ticket = owner.directory_copy_undo_ticket(reservation, &receipt).unwrap();
        assert!(owner.finish_directory_copy_undo(stale_ticket, reservation, &receipt, false));

        let (retry_reservation, _) = owner.reserve_undo().unwrap();
        let fresh_ticket = owner.directory_copy_undo_ticket(retry_reservation, &receipt).unwrap();
        assert_ne!(stale_ticket, fresh_ticket);
        assert!(!owner.finish_directory_copy_undo(stale_ticket, retry_reservation, &receipt, false),
            "late settlement from the previous reservation cannot release the fresh reservation");
        let slot = owner.directory_copy_undo_custody_slot();
        slot.install(DirectoryCopyUndoCustody::default());
        assert!(owner.finish_directory_copy_undo(fresh_ticket, retry_reservation, &receipt, true));
        assert!(!owner.runtime.undo_top_is_directory_copy(), "uncertain effect settlement consumes replay authority");
        assert!(owner.reserve_undo().is_none(), "unresolved passive custody blocks another directory Copy Undo");
    }

    #[test]
    fn directory_copy_group_ticket_rejects_stale_settlement_and_custody_blocks_replay() {
        use super::super::model::{DirectoryCopyUndoReceipt, SourceCleanupParentIdentity};
        let receipt = |name: &str| DirectoryCopyUndoReceipt {
            destination: std::path::PathBuf::from("target").join(name),
            destination_parent_route: vec![SourceCleanupParentIdentity {
                path: "target".into(), identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(1, 2),
            }],
            root_identity: format!("1:{name}"), entries: Vec::new(),
        };
        let receipts = vec![receipt("alpha"), receipt("beta")];
        let mut owner = OperationApplicationOwner::for_test();
        owner.runtime.push_undo_for_test(UndoEntry::DirectoryCopyGroup(receipts.clone()));
        let (reservation, _) = owner.reserve_undo().unwrap();
        let stale_ticket = owner.directory_copy_group_undo_ticket(reservation, &receipts).unwrap();
        assert!(owner.finish_directory_copy_group_undo(stale_ticket, reservation, &receipts, 1));
        let (retry_reservation, retry_entry) = owner.reserve_undo().unwrap();
        let UndoEntry::DirectoryCopyGroup(retained) = retry_entry else { panic!("one reverse suffix leaves the earlier item") };
        let ticket = owner.directory_copy_group_undo_ticket(retry_reservation, &retained).unwrap();
        assert!(!owner.finish_directory_copy_group_undo(stale_ticket, retry_reservation, &retained, 1), "stale group ticket is inert");
        assert!(!owner.finish_directory_copy_group_undo(ticket, retry_reservation, &retained, 2), "excess settlement is inert");
        owner.directory_copy_undo_custody_slot().install(super::super::executor::DirectoryCopyUndoCustody::default());
        assert!(owner.finish_directory_copy_group_undo(ticket, retry_reservation, &retained, 1));
        assert!(!owner.runtime.undo_top_is_directory_copy(), "settled effect cannot replay");
        assert!(owner.reserve_undo().is_none(), "single passive custody blocks another directory Copy Undo");
        assert!(!owner.finish_directory_copy_group_undo(ticket, retry_reservation, &retained, 1), "duplicate settlement is inert");
    }

    fn reopened_copy_stage_attempt(key: &str) -> JournalAttempt {
        use super::super::journal::{JournalCopyDirectoryStageEvidence, JournalPath};
        use super::super::model::{CopyDirectoryStageEvidence, CopyDirectoryStagePhase};
        let source_path = std::path::PathBuf::from(r"C:\source\folder");
        let parent_path = std::path::PathBuf::from(r"C:\target");
        let destination_path = parent_path.join("folder");
        let stage_path = parent_path.join(".gpui-filer-stage");
        let source = JournalPath::from_path(&source_path);
        let destination = JournalPath::from_path(&destination_path);
        let mut record = test_attempt(key);
        record.kind = super::super::journal::JournalKind::Copy;
        record.state = super::super::journal::JournalState::RecoveryRequired;
        record.sources = vec![source.clone()];
        record.destination = Some(JournalPath::from_path(&parent_path));
        record.terminal_outcome = Some(super::super::journal::JournalTerminalOutcome::Unknown);
        record.copy_directory_stage = Some(JournalCopyDirectoryStageEvidence {
            item: 0,
            source,
            destination: destination.clone(),
            evidence: CopyDirectoryStageEvidence {
                stage_path_units: JournalPath::from_path(&stage_path).units,
                destination_path_units: destination.units,
                expected_root_identity: "stage-root-id".into(),
                phase: CopyDirectoryStagePhase::StagingCancelled,
                detail: "cancelled after progress".into(),
            },
        });
        record
    }

    #[test]
    fn reopened_directory_move_diagnosis_is_exact_ticket_bound_and_discarded_with_record() {
        use super::super::model::{CopyInspectionState as State, ReopenedDirectoryMoveObservations as Observations, ReopenedDirectoryMoveDiagnosisState as DiagnosisState};
        let mut owner = OperationApplicationOwner::for_test();
        let record = reopened_directory_move_attempt("durable-key");
        owner.recovery_records.push(record);
        let (cancelled_ticket, _) = owner.begin_reopened_directory_move_diagnosis("durable-key").unwrap();
        owner.cancel_reopened_directory_move_diagnosis("durable-key", &cancelled_ticket);
        let (ticket, _) = owner.begin_reopened_directory_move_diagnosis("durable-key").unwrap();
        assert!(ticket.generation > cancelled_ticket.generation);
        let observations = Observations { source: State::Matches, destination: State::Changed };
        assert!(!owner.commit_reopened_directory_move_diagnosis("durable-key", &cancelled_ticket, observations));
        assert!(owner.commit_reopened_directory_move_diagnosis("durable-key", &ticket, observations));
        assert!(!owner.commit_reopened_directory_move_diagnosis("durable-key", &ticket, observations));
        assert!(!owner.commit_reopened_directory_move_diagnosis("wrong-key", &ticket, observations));
        assert!(matches!(owner.reopened_directory_move_diagnosis("durable-key").unwrap().1, DiagnosisState::Available(_)));
        let (pending_after_available, _) = owner.begin_reopened_directory_move_diagnosis("durable-key").unwrap();
        assert!(owner.discard_recovery_record("durable-key"));
        assert!(!owner.commit_reopened_directory_move_diagnosis("durable-key", &pending_after_available, observations));
        assert!(owner.reopened_directory_move_diagnosis("durable-key").is_none());
    }

    #[test]
    fn reopened_copy_stage_review_is_snapshot_generation_cancel_and_owner_bound() {
        use super::super::model::{CopyInspectionState as State, ReopenedCopyStageObservations as Observations, ReopenedCopyStageReviewState as ReviewState};
        let record = reopened_copy_stage_attempt("copy-key");
        let mut first = OperationApplicationOwner::for_test();
        first.recovery_records.push(record.clone());
        let (late_ticket, _) = first.begin_reopened_copy_stage_review("copy-key").unwrap();
        let (ticket, probe) = first.begin_reopened_copy_stage_review("copy-key").unwrap();
        assert!(ticket.generation > late_ticket.generation);
        assert_eq!(probe.expected_stage_identity.as_deref(), Some("stage-root-id"));
        let observations = Observations { stage: State::Matches, destination: State::Matches };
        assert!(!first.commit_reopened_copy_stage_review("copy-key", &late_ticket, observations));
        assert!(first.commit_reopened_copy_stage_review("copy-key", &ticket, observations));
        assert!(!first.commit_reopened_copy_stage_review("copy-key", &ticket, observations));
        assert!(matches!(first.reopened_copy_stage_review("copy-key").unwrap().1, ReviewState::Available(review)
            if review.stage_root_observation == State::Matches
                && review.destination_root_observation == State::Indeterminate
                && review.historical.evidence.phase == super::super::model::CopyDirectoryStagePhase::StagingCancelled));

        let (cancelled, _) = first.begin_reopened_copy_stage_review("copy-key").unwrap();
        first.cancel_reopened_copy_stage_review("copy-key", &cancelled);
        assert!(!first.commit_reopened_copy_stage_review("copy-key", &cancelled, observations));
        let (discarded, _) = first.begin_reopened_copy_stage_review("copy-key").unwrap();
        assert!(first.discard_recovery_record("copy-key"));
        assert!(!first.commit_reopened_copy_stage_review("copy-key", &discarded, observations));

        let mut replaced_owner = OperationApplicationOwner::for_test();
        replaced_owner.recovery_records.push(reopened_copy_stage_attempt("copy-key"));
        let (replaced_ticket, _) = replaced_owner.begin_reopened_copy_stage_review("copy-key").unwrap();
        replaced_owner.recovery_records[0].copy_directory_stage.as_mut().unwrap().evidence.detail = "replacement snapshot".into();
        assert!(!replaced_owner.commit_reopened_copy_stage_review("copy-key", &replaced_ticket, observations));

        let mut old_owner = OperationApplicationOwner::for_test();
        old_owner.recovery_records.push(record.clone());
        let (old_ticket, _) = old_owner.begin_reopened_copy_stage_review("copy-key").unwrap();
        let mut new_owner = OperationApplicationOwner::for_test();
        new_owner.recovery_records.push(record);
        let (new_ticket, _) = new_owner.begin_reopened_copy_stage_review("copy-key").unwrap();
        assert_ne!(old_ticket.owner_incarnation, new_ticket.owner_incarnation);
        assert!(!new_owner.commit_reopened_copy_stage_review("copy-key", &old_ticket, observations));
        assert!(new_owner.commit_reopened_copy_stage_review("copy-key", &new_ticket, observations));

        let mut missing_identity_record = reopened_copy_stage_attempt("copy-missing-stage-identity");
        missing_identity_record.copy_directory_stage.as_mut().unwrap().evidence.expected_root_identity.clear();
        let mut missing_identity_owner = OperationApplicationOwner::for_test();
        missing_identity_owner.recovery_records.push(missing_identity_record);
        let (missing_ticket, missing_probe) = missing_identity_owner
            .begin_reopened_copy_stage_review("copy-missing-stage-identity")
            .expect("missing historical identity remains reviewable");
        assert!(missing_probe.expected_stage_identity.is_none());
        assert!(missing_identity_owner.commit_reopened_copy_stage_review(
            "copy-missing-stage-identity",
            &missing_ticket,
            observations,
        ));
        assert!(matches!(
            missing_identity_owner.reopened_copy_stage_review("copy-missing-stage-identity").unwrap().1,
            ReviewState::Available(review) if review.stage_root_observation == State::Indeterminate
        ));

        let mut forged_identity_record = reopened_copy_stage_attempt("copy-unverified-with-identity");
        let evidence = &mut forged_identity_record.copy_directory_stage.as_mut().unwrap().evidence;
        evidence.phase = super::super::model::CopyDirectoryStagePhase::CreatedRootIdentityUnverified;
        evidence.expected_root_identity = "forged-root-id".into();
        let mut forged_identity_owner = OperationApplicationOwner::for_test();
        forged_identity_owner.recovery_records.push(forged_identity_record);
        let (forged_ticket, forged_probe) = forged_identity_owner
            .begin_reopened_copy_stage_review("copy-unverified-with-identity")
            .expect("crafted record still reaches the defensive application boundary");
        assert!(forged_probe.expected_stage_identity.is_none());
        assert!(forged_identity_owner.commit_reopened_copy_stage_review(
            "copy-unverified-with-identity",
            &forged_ticket,
            observations,
        ));
        assert!(matches!(
            forged_identity_owner.reopened_copy_stage_review("copy-unverified-with-identity").unwrap().1,
            ReviewState::Available(review) if review.stage_root_observation == State::Indeterminate
        ));
    }

    #[test]
    fn reopened_directory_move_diagnosis_rejects_stale_owner_and_missing_ids_are_indeterminate() {
        use super::super::model::{CopyInspectionState as State, ReopenedDirectoryMoveObservations as Observations, ReopenedDirectoryMoveDiagnosisState as DiagnosisState};
        let mut first = OperationApplicationOwner::for_test();
        let mut record = reopened_directory_move_attempt("durable-key");
        record.directory_moves[0].root.as_mut().unwrap().source_identity.clear();
        first.recovery_records.push(record);
        let (old_ticket, _) = first.begin_reopened_directory_move_diagnosis("durable-key").unwrap();
        let mut second = OperationApplicationOwner::for_test();
        second.recovery_records = first.recovery_records.clone();
        let (new_ticket, _) = second.begin_reopened_directory_move_diagnosis("durable-key").unwrap();
        assert_ne!(old_ticket.owner_incarnation, new_ticket.owner_incarnation);
        let observations = Observations { source: State::Matches, destination: State::Matches };
        assert!(!second.commit_reopened_directory_move_diagnosis("durable-key", &old_ticket, observations));
        assert!(second.commit_reopened_directory_move_diagnosis("durable-key", &new_ticket, observations));
        let Some((_, DiagnosisState::Available(diagnosis))) = second.reopened_directory_move_diagnosis("durable-key") else { panic!("available diagnosis") };
        assert_eq!(diagnosis.source_root_observation, State::Indeterminate);
        assert_eq!(diagnosis.destination_root_observation, State::Matches);
        second.cancel_reopened_directory_move_diagnosis("durable-key", &old_ticket);
        assert!(second.reopened_directory_move_diagnosis("durable-key").is_some());
    }

    #[test]
    fn reopened_directory_move_diagnosis_rejects_non_singleton_replace_and_bad_mapping() {
        let valid = reopened_directory_move_attempt("valid");
        let mut eligible_owner = OperationApplicationOwner::for_test();
        eligible_owner.recovery_records.push(valid.clone());
        assert!(eligible_owner.begin_reopened_directory_move_diagnosis("valid").is_some());
        let mut multi_source = valid.clone();
        multi_source.sources.push(multi_source.sources[0].clone());
        let mut multi_ledger = valid.clone();
        multi_ledger.directory_moves.push(multi_ledger.directory_moves[0].clone());
        let mut replacement = valid.clone();
        replacement.directory_moves[0].replace = Some(super::super::journal::JournalDirectoryMoveReplace {
            stage: replacement.directory_moves[0].destination.clone(), backup: replacement.directory_moves[0].destination.clone(),
            stage_identity: String::new(), approved_target_identity: String::new(), destination_parent_identity: String::new(), backup_root_identity: String::new(),
            backup_children: Vec::new(), backup_pending: None, backup_root_disposition_recorded: false, backup_cleanup_complete: false,
            phase: super::super::journal::DirectoryMoveReplacePhase::Staged,
        });
        let mut malformed_mapping = valid.clone();
        malformed_mapping.directory_moves[0].destination = malformed_mapping.sources[0].clone();
        let mut pathless = valid.clone();
        pathless.destination = None;
        for (key, mut record) in [("multi-source", multi_source), ("multi-ledger", multi_ledger), ("replace", replacement), ("mapping", malformed_mapping), ("pathless", pathless)] {
            record.key = key.to_owned();
            let mut owner = OperationApplicationOwner::for_test();
            owner.recovery_records.push(record);
            assert!(owner.begin_reopened_directory_move_diagnosis(key).is_none(), "{key} must remain unavailable");
        }
    }

    #[test]
    fn reopened_directory_move_diagnosis_waits_for_exact_discard_ack() {
        use super::super::model::{CopyInspectionState as State, ReopenedDirectoryMoveObservations as Observations};
        let mut owner = OperationApplicationOwner::for_test();
        let record = reopened_directory_move_attempt("durable-key");
        owner.recovery_records.push(record);
        let (writer, driver) = JournalWriter::test_driver();
        owner.journal = Some(writer);
        let (ticket, _) = owner.begin_reopened_directory_move_diagnosis("durable-key").unwrap();
        let ack = owner.journal.as_ref().unwrap().discard("durable-key".into());
        let command = driver.try_discard().expect("discard command");
        assert_eq!(command.key, "durable-key");
        assert!(matches!(owner.reopened_directory_move_diagnosis("durable-key").unwrap().1, super::super::model::ReopenedDirectoryMoveDiagnosisState::Pending));
        command.reply(Ok(true));
        assert!(matches!(ack.try_recv(), Ok(Ok(true))));
        assert!(owner.discard_recovery_record("durable-key"));
        assert!(!owner.commit_reopened_directory_move_diagnosis("durable-key", &ticket, Observations { source: State::Matches, destination: State::Matches }));
        assert!(owner.reopened_directory_move_diagnosis("durable-key").is_none());

        let mut failed_owner = OperationApplicationOwner::for_test();
        failed_owner.recovery_records.push(reopened_directory_move_attempt("retained-key"));
        let (writer, driver) = JournalWriter::test_driver();
        failed_owner.journal = Some(writer);
        let (retained_ticket, _) = failed_owner.begin_reopened_directory_move_diagnosis("retained-key").unwrap();
        let ack = failed_owner.journal.as_ref().unwrap().discard("retained-key".into());
        let command = driver.try_discard().expect("discard command");
        command.reply(Ok(false));
        assert!(matches!(ack.try_recv(), Ok(Ok(false))));
        assert_eq!(failed_owner.recovery_records.len(), 1);
        assert!(failed_owner.reopened_directory_move_diagnosis("retained-key").is_some());
        assert!(failed_owner.commit_reopened_directory_move_diagnosis("retained-key", &retained_ticket, Observations { source: State::Matches, destination: State::Matches }));
    }

    #[test]
    fn reopened_directory_move_diagnosis_starts_from_fresh_journal_owner_without_runtime_replay() {
        use std::fs;
        let root = tempfile::tempdir().unwrap();
        let journal_path = root.path().join("operations.json");
        let mut record = reopened_directory_move_attempt("reopened-move");
        let ledger = record.directory_moves.pop().unwrap();
        record.state = super::super::journal::JournalState::Running;
        let mut store = JournalStore::open(journal_path.clone()).unwrap();
        store.upsert(record.clone()).unwrap();
        let writer = JournalWriter::start(store);
        writer.register_directory_move(record.key.clone(), ledger.clone()).recv_blocking().unwrap().unwrap();
        record.directory_moves.push(ledger);
        record.state = super::super::journal::JournalState::RecoveryRequired;
        writer.upsert_with_reply(record.clone()).recv_blocking().unwrap().unwrap();
        drop(writer);
        let bytes = fs::read(&journal_path).unwrap();
        let mut first = OperationApplicationOwner::from_journal_store(Some(JournalStore::open(journal_path.clone()).unwrap()), None);
        assert_eq!(first.recovery_records, [record.clone()]);
        assert!(first.runtime.jobs().is_empty());
        let (ticket, probe) = first.begin_reopened_directory_move_diagnosis("reopened-move").unwrap();
        assert_eq!(ticket.key, record.key);
        assert_eq!(probe.expected_source_identity.as_deref(), Some("source-id"));
        let second = OperationApplicationOwner::from_journal_store(Some(JournalStore::open(journal_path.clone()).unwrap()), None);
        assert!(second.runtime.jobs().is_empty());
        assert!(second.reopened_directory_move_diagnosis("reopened-move").is_none());
        assert_eq!(fs::read(&journal_path).unwrap(), bytes);
        assert_eq!(first.recovery_records, second.recovery_records);
    }
}

/// Keeps broker shutdown aligned with the final Shell drop without giving a
/// Shell access to or ownership of the operation runtime.
pub(crate) struct OperationShellLease {
    live_shells: Rc<Cell<usize>>,
}

impl Drop for OperationShellLease {
    fn drop(&mut self) {
        let count = self
            .live_shells
            .get()
            .checked_sub(1)
            .expect("operation Shell lease count underflow");
        self.live_shells.set(count);
        if count == 0 {
            super::privileged::shutdown_session_broker();
        }
    }
}
