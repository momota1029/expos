use super::{
    JournalPath, JournalSevenZipExtractAttemptId, JournalSevenZipExtractFileBinding,
    JournalSevenZipExtractIdentity, JournalSevenZipExtractLedger,
    JournalSevenZipExtractObjectState,
};
use serde::{Deserialize, Serialize};

pub const SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractDescriptor {
    pub ordinal: u32,
    pub source_path: JournalPath,
    pub source_identity: JournalSevenZipExtractIdentity,
    pub output: JournalPath,
    pub spool: JournalPath,
    pub tree: JournalPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractEnvelope {
    pub attempt: JournalSevenZipExtractAttemptId,
    pub reservation_format: u32,
    pub provider_revision: u64,
    pub provider_executable: JournalSevenZipExtractFileBinding,
    pub provider_dll: JournalSevenZipExtractFileBinding,
    pub application_image: JournalSevenZipExtractFileBinding,
    pub destination_parent: JournalSevenZipExtractIdentity,
    pub items: Vec<JournalSevenZipExtractDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractBatch {
    pub envelope: JournalSevenZipExtractEnvelope,
    pub sequence: u64,
    pub items: Vec<JournalSevenZipExtractLedger>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalSevenZipExtractOutcome {
    Completed,
    Failed,
    CancelledBeforeStart,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractTerminalReceipt {
    pub ordinal: u32,
    pub outcome: JournalSevenZipExtractOutcome,
    pub output: Option<JournalPath>,
    pub publication_identity: Option<JournalSevenZipExtractIdentity>,
}

impl JournalSevenZipExtractEnvelope {
    pub fn validate(&self) -> Result<(), String> {
        if self.reservation_format != SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT
            || self.attempt.job == 0
            || self.attempt.attempt == 0
            || self.provider_revision == 0
            || self.items.is_empty()
            || self.items.len() > 1024
        {
            return Err("invalid 7z extraction batch envelope".into());
        }
        let mut paths = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if item.ordinal != index as u32
                || !super::seven_zip_extract::valid_absolute_path(&item.source_path)
                || !super::seven_zip_extract::valid_absolute_path(&item.output)
                || !super::seven_zip_extract::valid_absolute_path(&item.spool)
                || !super::seven_zip_extract::valid_absolute_path(&item.tree)
                || item.source_identity.volume == 0
                || item.source_identity.file_index == 0
            {
                return Err("invalid 7z extraction descriptor".into());
            }
            let source = item
                .source_path
                .to_path()
                .ok_or("7z source path cannot be represented")?;
            let expected_output = source
                .parent()
                .and_then(|parent| source.file_stem().map(|stem| parent.join(stem)));
            if expected_output
                .as_ref()
                .is_none_or(|path| JournalPath::from_path(path) != item.output)
                || !source
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("7z"))
                || item
                    .spool
                    .to_path()
                    .and_then(|path| path.parent().map(JournalPath::from_path))
                    .as_ref()
                    != source.parent().map(JournalPath::from_path).as_ref()
                || item
                    .tree
                    .to_path()
                    .and_then(|path| path.parent().map(JournalPath::from_path))
                    .as_ref()
                    != source.parent().map(JournalPath::from_path).as_ref()
            {
                return Err(
                    "7z extraction output and temporary paths must be fixed siblings".into(),
                );
            }
            if self
                .items
                .iter()
                .take(index)
                .any(|prior| prior.source_identity == item.source_identity)
            {
                return Err("7z extraction source identities must be distinct".into());
            }
            let descriptor_paths = [&item.source_path, &item.output, &item.spool, &item.tree];
            if descriptor_paths.iter().enumerate().any(|(position, path)| {
                descriptor_paths
                    .iter()
                    .skip(position + 1)
                    .any(|other| aliases(path, other))
            }) {
                return Err("7z extraction descriptor paths must be mutually distinct".into());
            }
            if paths.iter().any(|path: &JournalPath| {
                descriptor_paths
                    .iter()
                    .any(|candidate| aliases(path, candidate))
            }) {
                return Err("7z extraction batch paths alias".into());
            }
            paths.extend([
                item.source_path.clone(),
                item.output.clone(),
                item.spool.clone(),
                item.tree.clone(),
            ]);
        }
        for binding in [
            &self.provider_executable,
            &self.provider_dll,
            &self.application_image,
        ] {
            if !super::seven_zip_extract::valid_absolute_path(&binding.path)
                || binding.identity.volume == 0
                || binding.identity.file_index == 0
            {
                return Err("invalid 7z extraction provider binding".into());
            }
            if paths.iter().any(|path| aliases(path, &binding.path)) {
                return Err("7z extraction route aliases an item path".into());
            }
            paths.push(binding.path.clone());
        }
        if self.provider_executable.identity == self.provider_dll.identity
            || self.destination_parent.volume == 0
            || self.destination_parent.file_index == 0
        {
            return Err("invalid 7z extraction destination identity".into());
        }
        Ok(())
    }
}

impl JournalSevenZipExtractBatch {
    pub fn validate(&self) -> Result<(), String> {
        self.envelope.validate()?;
        if self.sequence == 0 || self.items.len() > self.envelope.items.len() {
            return Err("invalid 7z extraction barrier sequence".into());
        }
        let unresolved = self
            .items
            .iter()
            .filter(|ledger| {
                !matches!(
                    ledger.phase,
                    super::JournalSevenZipExtractPhase::Published
                        | super::JournalSevenZipExtractPhase::Discarded
                )
            })
            .count();
        if unresolved > 1 {
            return Err("7z extraction batch cannot hold multiple unresolved item intents".into());
        }
        let mut previous = None;
        for ledger in &self.items {
            ledger.validate()?;
            let ordinal = ledger.binding.item;
            if previous.is_some_and(|p| ordinal <= p) {
                return Err("7z extraction item ledgers must be sorted and sparse".into());
            }
            previous = Some(ordinal);
            let descriptor = self
                .envelope
                .items
                .get(ordinal as usize)
                .ok_or("7z extraction item is outside its envelope")?;
            let b = &ledger.binding;
            if b.attempt != self.envelope.attempt
                || b.provider_revision != self.envelope.provider_revision
                || b.provider_executable != self.envelope.provider_executable
                || b.provider_dll != self.envelope.provider_dll
                || b.application_image != self.envelope.application_image
                || b.destination_parent != self.envelope.destination_parent
                || b.source.path != descriptor.source_path
                || b.source.identity != descriptor.source_identity
                || b.output != descriptor.output
                || b.spool != descriptor.spool
                || b.tree != descriptor.tree
            {
                return Err("7z extraction ledger does not match its registered descriptor".into());
            }
        }
        Ok(())
    }

    pub fn is_registration(&self) -> bool {
        self.sequence == 1 && self.items.is_empty() && self.validate().is_ok()
    }

    pub fn is_observation_successor_of(&self, previous: &Self) -> bool {
        use super::JournalSevenZipExtractObjectState as State;
        use super::JournalSevenZipExtractPhase as Phase;
        if self.items.len() != previous.items.len() || !self.follows(previous) {
            return false;
        }
        let mut changed = None;
        for (old, next) in previous.items.iter().zip(&self.items) {
            if old == next {
                continue;
            }
            if changed.is_some() {
                return false;
            }
            let publication_observation = old.phase == Phase::PublishIntent
                && next.phase == Phase::Published
                && old.aborting == next.aborting
                && old.spool_state == next.spool_state
                && old.tree_state == State::Owned
                && next.tree_state == State::Published
                && next.tree_identity == old.tree_identity
                && next.publication_identity == old.tree_identity;
            let fields_same = old.binding == next.binding
                && old.spool_ready == next.spool_ready
                && old.tree_ready == next.tree_ready
                && (old.publication_identity == next.publication_identity
                    || publication_observation);
            if !fields_same {
                return false;
            }
            let spool_created = old.phase == Phase::SpoolCreateIntent
                && next.phase == Phase::SpoolOwned
                && old.spool_state == State::CreateIntent
                && next.spool_state == State::Owned
                && old.tree_state == next.tree_state
                && old.tree_identity == next.tree_identity
                && (old.spool_identity.is_none() || old.spool_identity == next.spool_identity);
            let tree_created = old.phase == Phase::TreeCreateIntent
                && next.phase == Phase::TreeOwned
                && old.tree_state == State::CreateIntent
                && next.tree_state == State::Owned
                && old.spool_state == next.spool_state
                && old.spool_identity == next.spool_identity
                && (old.tree_identity.is_none() || old.tree_identity == next.tree_identity);
            let observed_create =
                !old.aborting && !next.aborting && (spool_created || tree_created);
            let observed_publish = publication_observation;
            let abort_latch = !old.aborting
                && next.aborting
                && old.phase == next.phase
                && old.spool_state == next.spool_state
                && old.spool_identity == next.spool_identity
                && old.tree_state == next.tree_state
                && old.tree_identity == next.tree_identity;
            let phase_only_discard = next.phase == Phase::Discarded
                && old.phase != Phase::Discarded
                && old.spool_state == next.spool_state
                && old.spool_identity == next.spool_identity
                && old.tree_state == next.tree_state
                && old.tree_identity == next.tree_identity
                && matches!(
                    next.spool_state,
                    State::Deleted | State::CreateFailed | State::NeverCreated
                )
                && matches!(
                    next.tree_state,
                    State::Deleted | State::CreateFailed | State::NeverCreated
                );
            let observed_abort = old.aborting
                && next.aborting
                && ((spool_created || tree_created)
                    || (old.phase == next.phase
                        && observed_object(
                            old.spool_state,
                            next.spool_state,
                            &old.spool_identity,
                            &next.spool_identity,
                        )
                        && observed_object(
                            old.tree_state,
                            next.tree_state,
                            &old.tree_identity,
                            &next.tree_identity,
                        )
                        && (old.spool_state != next.spool_state
                            || old.spool_identity != next.spool_identity
                            || old.tree_state != next.tree_state
                            || old.tree_identity != next.tree_identity))
                    || phase_only_discard);
            if !observed_create && !observed_publish && !observed_abort && !abort_latch {
                return false;
            }
            changed = Some(());
        }
        changed.is_some()
    }

    pub fn is_cancel_cleanup_successor_of(&self, previous: &Self) -> bool {
        use super::JournalSevenZipExtractObjectState as State;
        use super::JournalSevenZipExtractPhase as Phase;
        if self.items.len() != previous.items.len() || !self.follows(previous) {
            return false;
        }
        let mut changed = false;
        for (old, next) in previous.items.iter().zip(&self.items) {
            if old == next {
                continue;
            }
            if changed
                || !next.aborting
                || next.phase != old.phase
                || next.binding != old.binding
                || next.spool_ready != old.spool_ready
                || next.tree_ready != old.tree_ready
                || next.publication_identity != old.publication_identity
            {
                return false;
            }
            let spool = old.spool_state == State::Owned
                && next.spool_state == State::DeleteIntent
                && old.spool_identity.is_some()
                && old.spool_identity == next.spool_identity
                && old.tree_state == next.tree_state
                && old.tree_identity == next.tree_identity;
            let tree = old.tree_state == State::Owned
                && next.tree_state == State::DeleteIntent
                && old.tree_identity.is_some()
                && old.tree_identity == next.tree_identity
                && old.spool_state == next.spool_state
                && old.spool_identity == next.spool_identity;
            if !(spool || tree) || matches!(old.phase, Phase::Published | Phase::Discarded) {
                return false;
            }
            changed = true;
        }
        changed
    }

    pub fn follows(&self, previous: &Self) -> bool {
        if self.envelope != previous.envelope
            || previous.sequence.checked_add(1) != Some(self.sequence)
            || self.validate().is_err()
            || previous.validate().is_err()
            || previous.is_registration() && self.items.is_empty()
        {
            return false;
        }
        let changed = self
            .items
            .iter()
            .zip(&previous.items)
            .position(|(a, b)| a != b);
        match changed {
            Some(index) if self.items.len() == previous.items.len() => {
                let mut expected = previous.items[index].clone();
                let next = &self.items[index];
                if next.follows(&expected)
                    && self.items.iter().enumerate().all(|(i, v)| {
                        i == index
                            || (i < index && *v == previous.items[i])
                            || (i > index && *v == previous.items[i])
                    })
                {
                    true
                } else {
                    expected.sequence = 0;
                    false
                }
            }
            None if self.items.len() == previous.items.len() + 1 => {
                let last = self.items.last().unwrap();
                previous.items.iter().all(|ledger| {
                    matches!(
                        ledger.phase,
                        super::JournalSevenZipExtractPhase::Published
                            | super::JournalSevenZipExtractPhase::Discarded
                    )
                }) && last.is_initial()
                    && previous.items.iter().zip(&self.items).all(|(a, b)| a == b)
                    && previous
                        .items
                        .last()
                        .is_none_or(|old| old.binding.item < last.binding.item)
            }
            _ => false,
        }
    }
}

pub fn validate_seven_zip_extract_receipts(
    receipts: &[JournalSevenZipExtractTerminalReceipt],
    requested: usize,
    items: &[JournalSevenZipExtractLedger],
) -> Result<(), String> {
    if receipts.len() != requested || requested == 0 || requested > 1024 {
        return Err("7z extraction terminal receipt count mismatch".into());
    }
    for (ordinal, receipt) in receipts.iter().enumerate() {
        if receipt.ordinal != ordinal as u32 {
            return Err("7z extraction terminal receipts are not in request order".into());
        }
        let ledger = items
            .iter()
            .find(|item| item.binding.item == receipt.ordinal);
        if receipt.publication_identity.is_some() && receipt.output.is_none()
            || receipt
                .output
                .as_ref()
                .is_some_and(|output| ledger.is_none_or(|ledger| output != &ledger.binding.output))
            || receipt
                .publication_identity
                .as_ref()
                .is_some_and(|identity| {
                    ledger.and_then(|ledger| ledger.publication_identity.as_ref()) != Some(identity)
                })
            || ledger.is_some_and(|ledger| ledger.publication_identity.is_some())
                && (receipt.output.as_ref() != ledger.map(|ledger| &ledger.binding.output)
                    || receipt.publication_identity.as_ref()
                        != ledger.and_then(|ledger| ledger.publication_identity.as_ref()))
        {
            return Err(
                "7z extraction terminal receipt loses or changes a known publication fact".into(),
            );
        }
        match (
            receipt.outcome,
            receipt.output.as_ref(),
            receipt.publication_identity.as_ref(),
            ledger,
        ) {
            (
                JournalSevenZipExtractOutcome::Completed,
                Some(output),
                Some(identity),
                Some(ledger),
            ) if output == &ledger.binding.output
                && ledger.phase == super::JournalSevenZipExtractPhase::Published
                && ledger.tree_state == JournalSevenZipExtractObjectState::Published
                && ledger.publication_identity.as_ref() == Some(identity) => {}
            (JournalSevenZipExtractOutcome::Failed, None, None, None) => {}
            (JournalSevenZipExtractOutcome::Failed, None, None, Some(ledger))
                if ledger.phase == super::JournalSevenZipExtractPhase::Discarded => {}
            (JournalSevenZipExtractOutcome::CancelledBeforeStart, None, None, None) => {}
            (JournalSevenZipExtractOutcome::CancelledBeforeStart, None, None, Some(ledger))
                if ledger.phase == super::JournalSevenZipExtractPhase::Discarded => {}
            (
                JournalSevenZipExtractOutcome::Partial | JournalSevenZipExtractOutcome::Unknown,
                _,
                _,
                _,
            ) => {}
            _ => return Err("7z extraction terminal receipt contradicts item facts".into()),
        }
    }
    Ok(())
}

fn observed_object(
    old: JournalSevenZipExtractObjectState,
    new: JournalSevenZipExtractObjectState,
    old_id: &Option<JournalSevenZipExtractIdentity>,
    new_id: &Option<JournalSevenZipExtractIdentity>,
) -> bool {
    old == new
        || matches!(
            (old, new),
            (
                JournalSevenZipExtractObjectState::CreateIntent,
                JournalSevenZipExtractObjectState::Owned
                    | JournalSevenZipExtractObjectState::CreateFailed
                    | JournalSevenZipExtractObjectState::NeverCreated
            ) | (
                JournalSevenZipExtractObjectState::Planned,
                JournalSevenZipExtractObjectState::NeverCreated
            ) | (
                JournalSevenZipExtractObjectState::DeleteIntent,
                JournalSevenZipExtractObjectState::Deleted
                    | JournalSevenZipExtractObjectState::Owned
            )
        ) && (old_id.is_none() || old_id == new_id)
}

fn aliases(a: &JournalPath, b: &JournalPath) -> bool {
    super::seven_zip_extract::aliases(a, b)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn id(file_index: u64) -> JournalSevenZipExtractIdentity {
        JournalSevenZipExtractIdentity {
            volume: 1,
            file_index,
        }
    }

    pub(crate) fn envelope(root: &std::path::Path, count: usize) -> JournalSevenZipExtractEnvelope {
        let mut items = Vec::new();
        for ordinal in 0..count {
            let source = root.join(format!("archive-{ordinal}.7z"));
            let output = root.join(format!("archive-{ordinal}"));
            items.push(JournalSevenZipExtractDescriptor {
                ordinal: ordinal as u32,
                source_path: JournalPath::from_path(&source),
                source_identity: id(100 + ordinal as u64),
                output: JournalPath::from_path(&output),
                spool: JournalPath::from_path(&root.join(format!(".spool-{ordinal}.tmp"))),
                tree: JournalPath::from_path(&root.join(format!(".tree-{ordinal}.tmp"))),
            });
        }
        JournalSevenZipExtractEnvelope {
            attempt: JournalSevenZipExtractAttemptId {
                run_id: [7; 16],
                job: 8,
                attempt: 9,
            },
            reservation_format: SEVEN_ZIP_EXTRACT_RESERVATION_FORMAT,
            provider_revision: 3,
            provider_executable: JournalSevenZipExtractFileBinding {
                path: JournalPath::from_path(&root.join("7z.exe")),
                identity: id(200),
            },
            provider_dll: JournalSevenZipExtractFileBinding {
                path: JournalPath::from_path(&root.join("7z.dll")),
                identity: id(201),
            },
            application_image: JournalSevenZipExtractFileBinding {
                path: JournalPath::from_path(&root.join("gpui-filer.exe")),
                identity: id(202),
            },
            destination_parent: id(203),
            items,
        }
    }

    pub(crate) fn initial_item(
        envelope: &JournalSevenZipExtractEnvelope,
        ordinal: usize,
    ) -> JournalSevenZipExtractLedger {
        let descriptor = &envelope.items[ordinal];
        let mut ledger: JournalSevenZipExtractLedger =
            super::super::seven_zip_extract_test_fixture();
        ledger.binding.attempt = envelope.attempt.clone();
        ledger.binding.item = ordinal as u32;
        ledger.binding.source.path = descriptor.source_path.clone();
        ledger.binding.source.identity = descriptor.source_identity.clone();
        ledger.binding.source.length = 12;
        ledger.binding.source.last_write_ticks = 14;
        ledger.binding.provider_revision = envelope.provider_revision;
        ledger.binding.provider_executable = envelope.provider_executable.clone();
        ledger.binding.provider_dll = envelope.provider_dll.clone();
        ledger.binding.application_image = envelope.application_image.clone();
        ledger.binding.destination_parent = envelope.destination_parent.clone();
        ledger.binding.output = descriptor.output.clone();
        ledger.binding.spool = descriptor.spool.clone();
        ledger.binding.tree = descriptor.tree.clone();
        ledger.sequence = 1;
        assert!(
            ledger.validate().is_ok(),
            "fixture must be a valid initial item intent"
        );
        ledger
    }

    #[test]
    fn registration_and_sparse_item_barriers_are_ordered_and_immutable() {
        let root = tempfile::tempdir().unwrap();
        let envelope = envelope(root.path(), 2);
        envelope.validate().unwrap();
        let registration = JournalSevenZipExtractBatch {
            envelope: envelope.clone(),
            sequence: 1,
            items: Vec::new(),
        };
        assert!(registration.is_registration());

        let first = initial_item(&envelope, 0);
        let mut item_intent = registration.clone();
        item_intent.sequence = 2;
        item_intent.items.push(first.clone());
        item_intent.validate().unwrap();
        assert!(item_intent.follows(&registration));

        let mut first_aborting = first.clone();
        first_aborting.sequence = 2;
        first_aborting.aborting = true;
        let mut first_abort_latch = item_intent.clone();
        first_abort_latch.sequence = 3;
        first_abort_latch.items[0] = first_aborting;
        assert!(first_abort_latch.follows(&item_intent));
        let mut first_no_spool = first.clone();
        first_no_spool.sequence = 3;
        first_no_spool.aborting = true;
        first_no_spool.spool_state = JournalSevenZipExtractObjectState::NeverCreated;
        let mut first_spool_observed = first_abort_latch.clone();
        first_spool_observed.sequence = 4;
        first_spool_observed.items[0] = first_no_spool;
        assert!(first_spool_observed.follows(&first_abort_latch));
        assert!(
            first_spool_observed.is_observation_successor_of(&first_abort_latch),
            "aborting Planned-to-NeverCreated is an observation only"
        );
        let mut first_no_tree = first_spool_observed.items[0].clone();
        first_no_tree.sequence = 4;
        first_no_tree.tree_state = JournalSevenZipExtractObjectState::NeverCreated;
        let mut first_tree_observed = first_spool_observed.clone();
        first_tree_observed.sequence = 5;
        first_tree_observed.items[0] = first_no_tree;
        assert!(first_tree_observed.follows(&first_spool_observed));
        let mut first_discarded = first_tree_observed.items[0].clone();
        first_discarded.sequence = 5;
        first_discarded.phase = super::super::JournalSevenZipExtractPhase::Discarded;
        let mut first_resolved = first_tree_observed.clone();
        first_resolved.sequence = 6;
        first_resolved.items[0] = first_discarded;
        first_resolved.validate().unwrap();
        assert!(first_resolved.follows(&first_tree_observed));
        assert!(
            first_resolved.is_observation_successor_of(&first_tree_observed),
            "phase-only Discarded completion is retained as an observation"
        );
        let second = initial_item(&envelope, 1);
        let mut both_registered = first_resolved.clone();
        both_registered.sequence = 7;
        both_registered.items.push(second.clone());
        both_registered.validate().unwrap();
        assert!(both_registered.follows(&first_resolved));
        let mut invalid_two_unresolved = item_intent.clone();
        invalid_two_unresolved.items.push(second.clone());
        assert!(invalid_two_unresolved.validate().is_err());
        assert!(!invalid_two_unresolved.follows(&item_intent));

        let mut advanced = first.clone();
        advanced.sequence = 2;
        advanced.phase = super::super::JournalSevenZipExtractPhase::SpoolOwned;
        advanced.spool_state = JournalSevenZipExtractObjectState::Owned;
        advanced.spool_identity = Some(id(300));
        let mut next = item_intent.clone();
        next.sequence = 3;
        next.items[0] = advanced.clone();
        next.validate().unwrap();
        assert!(next.follows(&item_intent));
        let mut second_advanced = both_registered.clone();
        second_advanced.sequence = 8;
        second_advanced.items[1].sequence = 2;
        second_advanced.items[1].phase = super::super::JournalSevenZipExtractPhase::SpoolOwned;
        second_advanced.items[1].spool_state = JournalSevenZipExtractObjectState::Owned;
        second_advanced.items[1].spool_identity = Some(id(301));
        assert!(second_advanced.follows(&both_registered));
        assert_eq!(second_advanced.items[0], first_resolved.items[0]);

        let mut erased = next.clone();
        erased.sequence += 1;
        erased.items.clear();
        assert!(!erased.follows(&next));
        let mut reordered = item_intent.clone();
        reordered.envelope.items.swap(0, 1);
        assert!(!reordered.validate().is_ok());
        let mut late_owned = first.clone();
        late_owned.sequence = 2;
        late_owned.phase = super::super::JournalSevenZipExtractPhase::SpoolOwned;
        late_owned.spool_state = JournalSevenZipExtractObjectState::Owned;
        late_owned.spool_identity = Some(id(302));
        let mut late_owned_batch = item_intent.clone();
        late_owned_batch.sequence = 3;
        late_owned_batch.items[0] = late_owned;
        assert!(
            late_owned_batch.is_observation_successor_of(&item_intent),
            "creation already in flight may be recorded after cancellation as observation-only"
        );
        assert!(
            !item_intent.is_observation_successor_of(&item_intent),
            "a duplicate barrier is not a successor observation"
        );

        let mut forged = item_intent.clone();
        forged.items[0].binding.source.length += 1;
        forged.sequence += 1;
        assert!(!forged.follows(&item_intent));
        let mut changed_envelope = item_intent.clone();
        changed_envelope.envelope.provider_revision += 1;
        changed_envelope.sequence += 1;
        assert!(!changed_envelope.follows(&item_intent));
    }

    #[test]
    fn exact_published_identity_is_an_observation_successor_after_cancel_or_failure() {
        use super::super::JournalSevenZipExtractIdentity;
        use super::super::{
            JournalSevenZipExtractObjectState as State, JournalSevenZipExtractPhase as Phase,
        };

        let root = tempfile::tempdir().unwrap();
        let envelope = envelope(root.path(), 1);
        let ledger = initial_item(&envelope, 0);
        let make_batch = |next: JournalSevenZipExtractLedger| {
            let sequence = next.sequence + 1;
            let batch = JournalSevenZipExtractBatch {
                envelope: envelope.clone(),
                sequence,
                items: vec![next],
            };
            batch.validate().unwrap();
            batch
        };
        let b2 = make_batch(ledger.clone());

        let mut spool_owned = ledger.clone();
        spool_owned.sequence = 2;
        spool_owned.phase = Phase::SpoolOwned;
        spool_owned.spool_state = State::Owned;
        spool_owned.spool_identity = Some(JournalSevenZipExtractIdentity {
            volume: 1,
            file_index: 501,
        });
        let b3 = make_batch(spool_owned.clone());
        let mut spool_ready = spool_owned.clone();
        spool_ready.sequence = 3;
        spool_ready.phase = Phase::SpoolReady;
        spool_ready.spool_ready = Some(super::super::JournalSevenZipExtractSpoolReady {
            plan_format: spool_ready.binding.plan.plan_format,
            plan_sha256: spool_ready.binding.plan.sha256,
            length: spool_ready.binding.plan.total_expanded_bytes,
        });
        let b4 = make_batch(spool_ready.clone());
        let mut tree_intent = spool_ready.clone();
        tree_intent.sequence = 4;
        tree_intent.phase = Phase::TreeCreateIntent;
        tree_intent.tree_state = State::CreateIntent;
        let b5 = make_batch(tree_intent.clone());
        let mut tree_owned = tree_intent.clone();
        tree_owned.sequence = 5;
        tree_owned.phase = Phase::TreeOwned;
        tree_owned.tree_state = State::Owned;
        tree_owned.tree_identity = Some(JournalSevenZipExtractIdentity {
            volume: 1,
            file_index: 502,
        });
        let b6 = make_batch(tree_owned.clone());
        let mut tree_ready = tree_owned.clone();
        tree_ready.sequence = 6;
        tree_ready.phase = Phase::TreeReady;
        tree_ready.tree_ready = Some(super::super::JournalSevenZipExtractTreeReady {
            plan_format: tree_ready.binding.plan.plan_format,
            plan_sha256: tree_ready.binding.plan.sha256,
            materialized_node_count: tree_ready.binding.plan.materialized_node_count,
            file_count: tree_ready.binding.plan.file_count,
            total_verified_bytes: tree_ready.binding.plan.total_expanded_bytes,
        });
        let b7 = make_batch(tree_ready.clone());
        let mut delete_intent = tree_ready.clone();
        delete_intent.sequence = 7;
        delete_intent.spool_state = State::DeleteIntent;
        let b8 = make_batch(delete_intent.clone());
        let mut spool_cleaned = delete_intent.clone();
        spool_cleaned.sequence = 8;
        spool_cleaned.spool_state = State::Deleted;
        spool_cleaned.phase = Phase::SpoolCleaned;
        let b9 = make_batch(spool_cleaned.clone());
        let mut publish_intent = spool_cleaned.clone();
        publish_intent.sequence = 9;
        publish_intent.phase = Phase::PublishIntent;
        let publish_batch = make_batch(publish_intent.clone());
        let mut published = publish_intent.clone();
        published.sequence = 10;
        published.phase = Phase::Published;
        published.tree_state = State::Published;
        published.publication_identity = published.tree_identity.clone();
        let published_batch = make_batch(published.clone());

        for (next, previous) in [
            (&b3, &b2),
            (&b4, &b3),
            (&b5, &b4),
            (&b6, &b5),
            (&b7, &b6),
            (&b8, &b7),
            (&b9, &b8),
            (&publish_batch, &b9),
        ] {
            assert!(next.follows(previous));
        }
        assert!(published_batch.is_observation_successor_of(&publish_batch));
        let mut forged = published_batch;
        forged.items[0].publication_identity = Some(JournalSevenZipExtractIdentity {
            volume: 1,
            file_index: 999,
        });
        assert!(!forged.is_observation_successor_of(&publish_batch));

        let mut tree_abort_latch = tree_intent.clone();
        tree_abort_latch.sequence += 1;
        tree_abort_latch.aborting = true;
        let abort_latch_batch = make_batch(tree_abort_latch.clone());
        assert!(abort_latch_batch.is_observation_successor_of(&b5));
        let mut aborting_tree_owned = tree_owned;
        aborting_tree_owned.sequence += 1;
        aborting_tree_owned.aborting = true;
        let aborting_tree_owned_batch = make_batch(aborting_tree_owned.clone());
        assert!(
            aborting_tree_owned_batch.is_observation_successor_of(&abort_latch_batch),
            "aborting TreeCreateIntent-to-TreeOwned preserves exact late identity"
        );
        assert_eq!(
            aborting_tree_owned_batch.items[0].tree_identity,
            Some(JournalSevenZipExtractIdentity {
                volume: 1,
                file_index: 502
            }),
            "the exact newly observed identity remains part of the snapshot"
        );
        assert_eq!(ledger, initial_item(&envelope, 0));
    }

    #[test]
    fn envelope_rejects_collisions_duplicate_identity_and_unfixed_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut value = envelope(root.path(), 2);
        value.items[1].source_identity = value.items[0].source_identity.clone();
        assert!(value.validate().is_err());
        value = envelope(root.path(), 2);
        value.items[1].output = value.items[0].output.clone();
        assert!(value.validate().is_err());
        value = envelope(root.path(), 2);
        value.items[0].tree = value.items[0].spool.clone();
        assert!(value.validate().is_err());
    }

    #[test]
    fn terminal_receipts_cover_unstarted_items_and_reject_forged_success() {
        let failed = JournalSevenZipExtractTerminalReceipt {
            ordinal: 0,
            outcome: JournalSevenZipExtractOutcome::Failed,
            output: None,
            publication_identity: None,
        };
        let cancelled = JournalSevenZipExtractTerminalReceipt {
            ordinal: 1,
            outcome: JournalSevenZipExtractOutcome::CancelledBeforeStart,
            output: None,
            publication_identity: None,
        };
        assert!(validate_seven_zip_extract_receipts(&[failed.clone(), cancelled], 2, &[]).is_ok());
        assert!(validate_seven_zip_extract_receipts(&[failed.clone()], 2, &[]).is_err());
        let forged_success = JournalSevenZipExtractTerminalReceipt {
            ordinal: 0,
            outcome: JournalSevenZipExtractOutcome::Completed,
            output: Some(JournalPath::from_path(std::path::Path::new(r"C:\out"))),
            publication_identity: Some(id(900)),
        };
        assert!(validate_seven_zip_extract_receipts(&[forged_success], 1, &[]).is_err());
    }
}
