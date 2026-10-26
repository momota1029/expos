use super::JournalPath;
use serde::{Deserialize, Serialize};

const MAX_PATH_UNITS: usize = 32_767;
const MAX_PLAN_ITEMS: u32 = 65_536;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[cfg(test)]
pub(crate) fn test_fixture() -> JournalSevenZipExtractLedger {
    repair_tests::fixture()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractIdentity {
    pub volume: u32,
    pub file_index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractAttemptId {
    pub run_id: [u8; 16],
    pub job: u64,
    pub attempt: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractSource {
    pub path: JournalPath,
    pub identity: JournalSevenZipExtractIdentity,
    /// 捕捉した長さと時刻は変更検出用の署名で、内容ハッシュではない。
    pub length: u64,
    pub last_write_ticks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractFileBinding {
    pub path: JournalPath,
    pub identity: JournalSevenZipExtractIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractPlan {
    /// format 1 は既存の正規検査エンコードと index/range/implicit node の導出を再利用する。
    /// digest と要約は照合用で、保持した plan の利用や custody を証明しない。別のエンコードを作らない。
    pub plan_format: u32,
    pub sha256: [u8; 32],
    pub indexed_record_count: u32,
    pub materialized_node_count: u32,
    pub file_count: u32,
    pub total_expanded_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractBinding {
    pub attempt: JournalSevenZipExtractAttemptId,
    pub item: u32,
    pub source: JournalSevenZipExtractSource,
    pub provider_revision: u64,
    pub provider_executable: JournalSevenZipExtractFileBinding,
    pub provider_dll: JournalSevenZipExtractFileBinding,
    pub application_image: JournalSevenZipExtractFileBinding,
    pub destination_parent: JournalSevenZipExtractIdentity,
    pub output: JournalPath,
    pub spool: JournalPath,
    pub tree: JournalPath,
    pub plan: JournalSevenZipExtractPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalSevenZipExtractObjectState {
    Planned,
    CreateIntent,
    Owned,
    CreateFailed,
    NeverCreated,
    DeleteIntent,
    Deleted,
    Published,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalSevenZipExtractPhase {
    SpoolCreateIntent,
    SpoolOwned,
    SpoolReady,
    TreeCreateIntent,
    TreeOwned,
    TreeReady,
    SpoolCleaned,
    PublishIntent,
    Published,
    Discarded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractSpoolReady {
    pub plan_format: u32,
    pub plan_sha256: [u8; 32],
    pub length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractTreeReady {
    pub plan_format: u32,
    pub plan_sha256: [u8; 32],
    pub materialized_node_count: u32,
    pub file_count: u32,
    pub total_verified_bytes: u64,
}

/// 照合用の観測事実だけを持ち、ファイル操作や削除の権限にはならない。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalSevenZipExtractLedger {
    pub binding: JournalSevenZipExtractBinding,
    pub sequence: u64,
    pub aborting: bool,
    pub phase: JournalSevenZipExtractPhase,
    pub spool_state: JournalSevenZipExtractObjectState,
    pub spool_identity: Option<JournalSevenZipExtractIdentity>,
    pub tree_state: JournalSevenZipExtractObjectState,
    pub tree_identity: Option<JournalSevenZipExtractIdentity>,
    pub spool_ready: Option<JournalSevenZipExtractSpoolReady>,
    pub tree_ready: Option<JournalSevenZipExtractTreeReady>,
    pub publication_identity: Option<JournalSevenZipExtractIdentity>,
}

impl JournalSevenZipExtractLedger {
    pub fn is_initial(&self) -> bool {
        self.validate().is_ok() && self.raw_initial()
    }

    fn raw_initial(&self) -> bool {
        self.sequence == 1
            && !self.aborting
            && self.phase == JournalSevenZipExtractPhase::SpoolCreateIntent
            && self.spool_state == JournalSevenZipExtractObjectState::CreateIntent
            && self.spool_identity.is_none()
            && self.tree_state == JournalSevenZipExtractObjectState::Planned
            && self.tree_identity.is_none()
            && self.spool_ready.is_none()
            && self.tree_ready.is_none()
            && self.publication_identity.is_none()
    }

    pub fn validate(&self) -> Result<(), String> {
        let b = &self.binding;
        if b.attempt.job == 0 || b.attempt.attempt == 0 || b.provider_revision == 0 {
            return Err("7z extract attempt and provider revision must be nonzero".into());
        }
        if self.sequence == 0 {
            return Err("7z extract sequence must be nonzero".into());
        }
        if !valid_absolute_path(&b.source.path)
            || !valid_absolute_path(&b.provider_executable.path)
            || !valid_absolute_path(&b.provider_dll.path)
            || !valid_absolute_path(&b.application_image.path)
            || !valid_absolute_path(&b.output)
            || !valid_absolute_path(&b.spool)
            || !valid_absolute_path(&b.tree)
        {
            return Err("7z extract binding contains an invalid absolute path".into());
        }
        if b.plan.plan_format != 1 {
            return Err("unsupported 7z extract plan format".into());
        }
        let p = &b.plan;
        if p.indexed_record_count > MAX_PLAN_ITEMS
            || p.materialized_node_count > MAX_PLAN_ITEMS
            || p.file_count > p.indexed_record_count
            || p.file_count > p.materialized_node_count
            || p.indexed_record_count > p.materialized_node_count
            || p.total_expanded_bytes > MAX_TOTAL_BYTES
            || p.total_expanded_bytes
                > u64::from(p.file_count)
                    .checked_mul(MAX_FILE_BYTES)
                    .unwrap_or(u64::MAX)
            || (p.file_count == 0 && p.total_expanded_bytes != 0)
            || (p.indexed_record_count == 0 && p.materialized_node_count != 0)
        {
            return Err("7z extract plan counts or byte bounds are inconsistent".into());
        }
        if !same_parent(&b.output, &b.spool)
            || !same_parent(&b.output, &b.tree)
            || aliases(&b.output, &b.spool)
            || aliases(&b.output, &b.tree)
            || aliases(&b.spool, &b.tree)
        {
            return Err(
                "7z extract output, spool, and tree paths alias or differ in parent".into(),
            );
        }
        let source_id = &b.source.identity;
        let static_ids = [
            source_id,
            &b.provider_executable.identity,
            &b.provider_dll.identity,
            &b.application_image.identity,
        ];
        for observed in [self.spool_identity.as_ref(), self.tree_identity.as_ref()]
            .into_iter()
            .flatten()
        {
            if static_ids.iter().any(|id| *id == observed)
                || observed.volume != b.destination_parent.volume
            {
                return Err("7z extract observed identity aliases a bound object or volume".into());
            }
        }
        if self.spool_identity.is_some()
            && self.tree_identity.is_some()
            && self.spool_identity == self.tree_identity
        {
            return Err("7z extract spool and tree identities must be distinct".into());
        }
        if !valid_object_identity(self.spool_state, self.spool_identity.is_some(), false)
            || !valid_object_identity(self.tree_state, self.tree_identity.is_some(), true)
        {
            return Err("7z extract object state disagrees with its identity".into());
        }
        if let Some(receipt) = &self.spool_ready {
            if receipt.plan_format != p.plan_format
                || receipt.plan_sha256 != p.sha256
                || receipt.length != p.total_expanded_bytes
            {
                return Err("7z extract spool readiness receipt mismatches plan".into());
            }
        }
        if let Some(receipt) = &self.tree_ready {
            if receipt.plan_format != p.plan_format
                || receipt.plan_sha256 != p.sha256
                || receipt.materialized_node_count != p.materialized_node_count
                || receipt.file_count != p.file_count
                || receipt.total_verified_bytes != p.total_expanded_bytes
            {
                return Err("7z extract tree readiness receipt mismatches plan".into());
            }
        }
        if (self.spool_ready.is_some()
            && self.spool_state == JournalSevenZipExtractObjectState::Planned)
            || (self.tree_ready.is_some()
                && self.tree_state == JournalSevenZipExtractObjectState::Planned)
        {
            return Err("7z extract readiness precedes object creation".into());
        }
        if self.aborting && self.phase != JournalSevenZipExtractPhase::PublishIntent {
            if !valid_aborting_phase(self) {
                return Err("aborting 7z extract snapshot disagrees with historical phase".into());
            }
        } else {
            match self.phase {
                JournalSevenZipExtractPhase::SpoolCreateIntent => {
                    if self.spool_state != JournalSevenZipExtractObjectState::CreateIntent
                        || self.spool_ready.is_some()
                        || self.tree_state != JournalSevenZipExtractObjectState::Planned
                    {
                        return Err("invalid SpoolCreateIntent snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::SpoolOwned => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Owned
                        || self.spool_ready.is_some()
                        || self.tree_state != JournalSevenZipExtractObjectState::Planned
                    {
                        return Err("invalid SpoolOwned snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::SpoolReady => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Owned
                        || self.spool_ready.is_none()
                        || self.tree_state != JournalSevenZipExtractObjectState::Planned
                    {
                        return Err("invalid SpoolReady snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::TreeCreateIntent => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Owned
                        || self.spool_ready.is_none()
                        || self.tree_state != JournalSevenZipExtractObjectState::CreateIntent
                    {
                        return Err("invalid TreeCreateIntent snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::TreeOwned => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Owned
                        || self.spool_ready.is_none()
                        || self.tree_state != JournalSevenZipExtractObjectState::Owned
                    {
                        return Err("invalid TreeOwned snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::TreeReady => {
                    if !matches!(
                        self.spool_state,
                        JournalSevenZipExtractObjectState::Owned
                            | JournalSevenZipExtractObjectState::DeleteIntent
                    ) || self.spool_ready.is_none()
                        || self.tree_state != JournalSevenZipExtractObjectState::Owned
                        || self.tree_ready.is_none()
                    {
                        return Err("invalid TreeReady snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::SpoolCleaned => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Deleted
                        || self.tree_state != JournalSevenZipExtractObjectState::Owned
                        || self.spool_ready.is_none()
                        || self.tree_ready.is_none()
                    {
                        return Err("invalid SpoolCleaned snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::PublishIntent => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Deleted
                        || self.tree_state != JournalSevenZipExtractObjectState::Owned
                        || self.tree_ready.is_none()
                        || self.publication_identity.is_some()
                    {
                        return Err("invalid PublishIntent snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::Published => {
                    if self.spool_state != JournalSevenZipExtractObjectState::Deleted
                        || self.tree_state != JournalSevenZipExtractObjectState::Published
                        || self.tree_ready.is_none()
                        || self.publication_identity.is_none()
                        || self.publication_identity.as_ref() != self.tree_identity.as_ref()
                    {
                        return Err("invalid Published snapshot".into());
                    }
                }
                JournalSevenZipExtractPhase::Discarded => {
                    if !self.aborting
                        || self.publication_identity.is_some()
                        || matches!(
                            self.spool_state,
                            JournalSevenZipExtractObjectState::Owned
                                | JournalSevenZipExtractObjectState::DeleteIntent
                                | JournalSevenZipExtractObjectState::CreateIntent
                                | JournalSevenZipExtractObjectState::Planned
                        )
                        || matches!(
                            self.tree_state,
                            JournalSevenZipExtractObjectState::Owned
                                | JournalSevenZipExtractObjectState::DeleteIntent
                                | JournalSevenZipExtractObjectState::CreateIntent
                                | JournalSevenZipExtractObjectState::Planned
                                | JournalSevenZipExtractObjectState::Published
                        )
                    {
                        return Err("invalid Discarded snapshot".into());
                    }
                }
            }
        }
        if self.publication_identity.is_some()
            && self.phase != JournalSevenZipExtractPhase::Published
        {
            return Err("publication identity exists outside Published".into());
        }
        match self.phase {
            JournalSevenZipExtractPhase::SpoolCreateIntent
            | JournalSevenZipExtractPhase::SpoolOwned => {
                if self.spool_ready.is_some() || self.tree_ready.is_some() {
                    return Err("readiness receipt precedes its phase".into());
                }
            }
            JournalSevenZipExtractPhase::SpoolReady
            | JournalSevenZipExtractPhase::TreeCreateIntent
            | JournalSevenZipExtractPhase::TreeOwned => {
                if self.spool_ready.is_none() || self.tree_ready.is_some() {
                    return Err("readiness receipt disagrees with historical phase".into());
                }
            }
            JournalSevenZipExtractPhase::TreeReady
            | JournalSevenZipExtractPhase::SpoolCleaned
            | JournalSevenZipExtractPhase::PublishIntent
            | JournalSevenZipExtractPhase::Published => {
                if self.spool_ready.is_none() || self.tree_ready.is_none() {
                    return Err("published tree requires both retained readiness receipts".into());
                }
            }
            JournalSevenZipExtractPhase::Discarded => {
                if self.tree_ready.is_some() && self.spool_ready.is_none() {
                    return Err("tree readiness cannot exist without spool readiness".into());
                }
            }
        }
        if (self.spool_ready.is_some() && self.spool_identity.is_none())
            || (self.tree_ready.is_some() && self.tree_identity.is_none())
        {
            return Err("readiness receipt lacks an observed object identity".into());
        }
        if self.sequence == 1 && !self.raw_initial() {
            return Err("sequence one is reserved for the initial snapshot".into());
        }
        Ok(())
    }

    /// 受領記録だけでは削除権限を作らない。後続の ACK と Job drain が別に必要。
    pub fn follows(&self, previous: &Self) -> bool {
        if self == previous {
            return self.validate().is_ok();
        }
        if previous.validate().is_err()
            || self.validate().is_err()
            || previous.sequence.checked_add(1) != Some(self.sequence)
            || self.binding != previous.binding
        {
            return false;
        }
        if previous.phase == JournalSevenZipExtractPhase::Published
            || previous.phase == JournalSevenZipExtractPhase::Discarded
        {
            return false;
        }

        if !previous.aborting && self.aborting {
            let mut expected = previous.clone();
            expected.sequence = self.sequence;
            expected.aborting = true;
            return *self == expected;
        }
        if previous.aborting && !self.aborting {
            return false;
        }

        // PublishIntent は公開成否が不明なため、後続は公開観測だけに絞る。
        if previous.phase == JournalSevenZipExtractPhase::PublishIntent {
            let mut expected = previous.clone();
            expected.sequence = self.sequence;
            expected.phase = JournalSevenZipExtractPhase::Published;
            expected.tree_state = JournalSevenZipExtractObjectState::Published;
            expected.publication_identity = previous.tree_identity.clone();
            return self == &expected;
        }

        let mut expected = previous.clone();
        expected.sequence = self.sequence;
        let valid = if previous.aborting {
            abort_step(&mut expected, self)
        } else {
            normal_step(&mut expected, self)
        };
        valid && *self == expected
    }
}

#[cfg(test)]
mod repair_tests {
    use super::*;
    use JournalSevenZipExtractObjectState as O;
    use JournalSevenZipExtractPhase as P;

    fn path(s: &str) -> JournalPath {
        JournalPath {
            units: s.encode_utf16().collect(),
        }
    }
    fn id(volume: u32, file_index: u64) -> JournalSevenZipExtractIdentity {
        JournalSevenZipExtractIdentity { volume, file_index }
    }
    fn plan(files: u32, records: u32, nodes: u32, bytes: u64) -> JournalSevenZipExtractPlan {
        JournalSevenZipExtractPlan {
            plan_format: 1,
            sha256: [9; 32],
            indexed_record_count: records,
            materialized_node_count: nodes,
            file_count: files,
            total_expanded_bytes: bytes,
        }
    }
    fn initial(plan: JournalSevenZipExtractPlan) -> JournalSevenZipExtractLedger {
        JournalSevenZipExtractLedger {
            binding: JournalSevenZipExtractBinding {
                attempt: JournalSevenZipExtractAttemptId {
                    run_id: [1; 16],
                    job: 2,
                    attempt: 3,
                },
                item: 4,
                source: JournalSevenZipExtractSource {
                    path: path(r"C:\archive.7z"),
                    identity: id(1, 1),
                    length: 99,
                    last_write_ticks: 77,
                },
                provider_revision: 5,
                provider_executable: JournalSevenZipExtractFileBinding {
                    path: path(r"C:\7zip\7z.exe"),
                    identity: id(1, 2),
                },
                provider_dll: JournalSevenZipExtractFileBinding {
                    path: path(r"C:\7zip\7z.dll"),
                    identity: id(1, 3),
                },
                application_image: JournalSevenZipExtractFileBinding {
                    path: path(r"C:\app\helper.exe"),
                    identity: id(1, 4),
                },
                destination_parent: id(2, 10),
                output: path(r"D:\out"),
                spool: path(r"D:\.spool"),
                tree: path(r"D:\.tree"),
                plan,
            },
            sequence: 1,
            aborting: false,
            phase: P::SpoolCreateIntent,
            spool_state: O::CreateIntent,
            spool_identity: None,
            tree_state: O::Planned,
            tree_identity: None,
            spool_ready: None,
            tree_ready: None,
            publication_identity: None,
        }
    }

    pub(crate) fn fixture() -> JournalSevenZipExtractLedger {
        initial(plan(0, 0, 0, 0))
    }
    fn spool_ready(p: &JournalSevenZipExtractPlan) -> JournalSevenZipExtractSpoolReady {
        JournalSevenZipExtractSpoolReady {
            plan_format: p.plan_format,
            plan_sha256: p.sha256,
            length: p.total_expanded_bytes,
        }
    }
    fn tree_ready(p: &JournalSevenZipExtractPlan) -> JournalSevenZipExtractTreeReady {
        JournalSevenZipExtractTreeReady {
            plan_format: p.plan_format,
            plan_sha256: p.sha256,
            materialized_node_count: p.materialized_node_count,
            file_count: p.file_count,
            total_verified_bytes: p.total_expanded_bytes,
        }
    }
    fn owned_spool(s: &JournalSevenZipExtractLedger, seq: u64) -> JournalSevenZipExtractLedger {
        let mut n = s.clone();
        n.sequence = seq;
        n.phase = P::SpoolOwned;
        n.spool_state = O::Owned;
        n.spool_identity = Some(id(2, 11));
        n
    }
    fn ready_spool(s: &JournalSevenZipExtractLedger, seq: u64) -> JournalSevenZipExtractLedger {
        let mut n = s.clone();
        n.sequence = seq;
        n.phase = P::SpoolReady;
        n.spool_ready = Some(spool_ready(&n.binding.plan));
        n
    }
    fn tree_intent(s: &JournalSevenZipExtractLedger, seq: u64) -> JournalSevenZipExtractLedger {
        let mut n = s.clone();
        n.sequence = seq;
        n.phase = P::TreeCreateIntent;
        n.tree_state = O::CreateIntent;
        n
    }
    fn owned_tree(s: &JournalSevenZipExtractLedger, seq: u64) -> JournalSevenZipExtractLedger {
        let mut n = s.clone();
        n.sequence = seq;
        n.phase = P::TreeOwned;
        n.tree_state = O::Owned;
        n.tree_identity = Some(id(2, 12));
        n
    }
    fn ready_tree(s: &JournalSevenZipExtractLedger, seq: u64) -> JournalSevenZipExtractLedger {
        let mut n = s.clone();
        n.sequence = seq;
        n.phase = P::TreeReady;
        n.tree_ready = Some(tree_ready(&n.binding.plan));
        n
    }
    fn asserted_transition(
        previous: &JournalSevenZipExtractLedger,
        next: &JournalSevenZipExtractLedger,
    ) {
        assert!(
            previous.validate().is_ok(),
            "invalid previous {:?}",
            previous
        );
        assert!(next.validate().is_ok(), "invalid next {:?}", next);
        assert!(
            next.follows(previous),
            "rejected expected transition {:?} -> {:?}",
            previous,
            next
        );
    }
    fn invalid_transition(
        previous: &JournalSevenZipExtractLedger,
        next: &JournalSevenZipExtractLedger,
    ) {
        assert!(
            !next.follows(previous),
            "accepted invalid transition {:?} -> {:?}",
            previous,
            next
        );
    }

    fn through_tree_ready() -> Vec<JournalSevenZipExtractLedger> {
        let a = initial(plan(0, 0, 0, 0));
        let b = owned_spool(&a, 2);
        let c = ready_spool(&b, 3);
        let d = tree_intent(&c, 4);
        let e = owned_tree(&d, 5);
        let f = ready_tree(&e, 6);
        vec![a, b, c, d, e, f]
    }

    #[test]
    fn empty_and_zero_byte_plans_validate_and_full_normal_lifecycle_roundtrips() {
        let mut snapshots = through_tree_ready();
        let f = snapshots.last().unwrap().clone();
        let mut delete_intent = f.clone();
        delete_intent.sequence = 7;
        delete_intent.spool_state = O::DeleteIntent;
        asserted_transition(&f, &delete_intent);
        let mut spool_deleted = delete_intent.clone();
        spool_deleted.sequence = 8;
        spool_deleted.spool_state = O::Deleted;
        spool_deleted.phase = P::SpoolCleaned;
        asserted_transition(&delete_intent, &spool_deleted);
        let mut publish = spool_deleted.clone();
        publish.sequence = 9;
        publish.phase = P::PublishIntent;
        asserted_transition(&spool_deleted, &publish);
        let mut published = publish.clone();
        published.sequence = 10;
        published.phase = P::Published;
        published.tree_state = O::Published;
        published.publication_identity = published.tree_identity.clone();
        asserted_transition(&publish, &published);
        snapshots.extend([delete_intent, spool_deleted, publish, published]);
        for s in &snapshots {
            assert!(s.is_initial() == (s.sequence == 1));
            assert!(
                s.follows(s),
                "valid exact duplicate rejected at {:?}",
                s.phase
            );
            let bytes = serde_json::to_vec(s).unwrap();
            assert_eq!(
                serde_json::from_slice::<JournalSevenZipExtractLedger>(&bytes).unwrap(),
                *s
            );
        }
        let zero_plan = plan(1, 1, 1, 0);
        let z1 = initial(zero_plan);
        let z2 = owned_spool(&z1, 2);
        asserted_transition(&z1, &z2);
        let z3 = ready_spool(&z2, 3);
        asserted_transition(&z2, &z3);
        let z4 = tree_intent(&z3, 4);
        asserted_transition(&z3, &z4);
        let z5 = owned_tree(&z4, 5);
        asserted_transition(&z4, &z5);
        let z6 = ready_tree(&z5, 6);
        asserted_transition(&z5, &z6);
        let mut z7 = z6.clone();
        z7.sequence = 7;
        z7.spool_state = O::DeleteIntent;
        asserted_transition(&z6, &z7);
        let mut z8 = z7.clone();
        z8.sequence = 8;
        z8.spool_state = O::Deleted;
        z8.phase = P::SpoolCleaned;
        asserted_transition(&z7, &z8);
        let mut z9 = z8.clone();
        z9.sequence = 9;
        z9.phase = P::PublishIntent;
        asserted_transition(&z8, &z9);
        let mut z10 = z9.clone();
        z10.sequence = 10;
        z10.phase = P::Published;
        z10.tree_state = O::Published;
        z10.publication_identity = z10.tree_identity.clone();
        asserted_transition(&z9, &z10);
    }

    #[test]
    fn every_prepublication_snapshot_accepts_only_a_standalone_abort_latch() {
        let snapshots = through_tree_ready();
        let mut delete_intent = snapshots.last().unwrap().clone();
        delete_intent.sequence = 7;
        delete_intent.spool_state = O::DeleteIntent;
        let mut spool_deleted = delete_intent.clone();
        spool_deleted.sequence = 8;
        spool_deleted.spool_state = O::Deleted;
        spool_deleted.phase = P::SpoolCleaned;
        let mut publish = spool_deleted.clone();
        publish.sequence = 9;
        publish.phase = P::PublishIntent;
        let mut phases = snapshots;
        phases.extend([delete_intent, spool_deleted, publish]);
        for current in phases {
            let mut latch = current.clone();
            latch.sequence += 1;
            latch.aborting = true;
            asserted_transition(&current, &latch);
            let mut cleanup = latch.clone();
            cleanup.sequence += 1;
            match latch.phase {
                P::SpoolCreateIntent => {
                    cleanup.phase = P::SpoolOwned;
                    cleanup.spool_state = O::Owned;
                    cleanup.spool_identity = Some(id(2, 11));
                }
                P::SpoolOwned | P::SpoolReady if latch.spool_state == O::Owned => {
                    cleanup.spool_state = O::DeleteIntent
                }
                P::TreeCreateIntent => {
                    cleanup.phase = P::TreeOwned;
                    cleanup.tree_state = O::Owned;
                    cleanup.tree_identity = Some(id(2, 12));
                }
                P::TreeOwned if latch.tree_state == O::Owned => {
                    cleanup.tree_state = O::DeleteIntent
                }
                P::TreeReady if latch.spool_state == O::Owned => {
                    cleanup.spool_state = O::DeleteIntent
                }
                P::TreeReady if latch.spool_state == O::DeleteIntent => {
                    cleanup.spool_state = O::Deleted
                }
                P::SpoolCleaned => cleanup.tree_state = O::DeleteIntent,
                P::PublishIntent => {
                    cleanup.phase = P::Published;
                    cleanup.tree_state = O::Published;
                    cleanup.publication_identity = cleanup.tree_identity.clone();
                }
                _ => panic!("no abort action for {:?}", latch.phase),
            }
            asserted_transition(&latch, &cleanup);
            let mut changed = latch.clone();
            changed.sequence += 1;
            changed.spool_ready = None;
            invalid_transition(&latch, &changed);
            assert!(latch.follows(&latch), "valid abort duplicate rejected");
            if latch.phase == P::SpoolOwned {
                let mut readiness = latch.clone();
                readiness.sequence += 1;
                readiness.phase = P::SpoolReady;
                readiness.spool_ready = Some(spool_ready(&readiness.binding.plan));
                invalid_transition(&latch, &readiness);
            }
            if latch.phase == P::TreeOwned {
                let mut readiness = latch.clone();
                readiness.sequence += 1;
                readiness.phase = P::TreeReady;
                readiness.tree_ready = Some(tree_ready(&readiness.binding.plan));
                invalid_transition(&latch, &readiness);
            }
            let mut reverse = latch.clone();
            reverse.sequence += 1;
            reverse.aborting = false;
            invalid_transition(&latch, &reverse);
        }
    }

    #[test]
    fn late_ownership_for_both_objects_and_cleanup_are_independent_and_ordered() {
        // spool を保持したままでも、中断直後に tree の所有観測が届く場合がある。
        let mut ti = through_tree_ready()[3].clone();
        ti.sequence = 5;
        ti.aborting = true;
        let mut late_tree = ti.clone();
        late_tree.sequence = 6;
        late_tree.phase = P::TreeOwned;
        late_tree.tree_state = O::Owned;
        late_tree.tree_identity = Some(id(2, 12));
        asserted_transition(&ti, &late_tree);
        let mut tree_delete = late_tree.clone();
        tree_delete.sequence = 7;
        tree_delete.tree_state = O::DeleteIntent;
        asserted_transition(&late_tree, &tree_delete);
        let mut tree_deleted = tree_delete.clone();
        tree_deleted.sequence = 8;
        tree_deleted.tree_state = O::Deleted;
        asserted_transition(&tree_delete, &tree_deleted);
        let mut spool_delete = tree_deleted.clone();
        spool_delete.sequence = 9;
        spool_delete.spool_state = O::DeleteIntent;
        asserted_transition(&tree_deleted, &spool_delete);
        let mut spool_deleted = spool_delete.clone();
        spool_deleted.sequence = 10;
        spool_deleted.spool_state = O::Deleted;
        asserted_transition(&spool_delete, &spool_deleted);
        let mut discard = spool_deleted.clone();
        discard.sequence = 11;
        discard.phase = P::Discarded;
        asserted_transition(&spool_deleted, &discard);
        invalid_transition(&discard, &{
            let mut n = discard.clone();
            n.sequence += 1;
            n
        });

        // 遅れた spool 作成結果も保持し、削除後に tree 未作成の観測を記録する。
        let a = initial(plan(0, 0, 0, 0));
        let mut aborted = a.clone();
        aborted.sequence = 2;
        aborted.aborting = true;
        asserted_transition(&a, &aborted);
        let mut late_spool = aborted.clone();
        late_spool.sequence = 3;
        late_spool.phase = P::SpoolOwned;
        late_spool.spool_state = O::Owned;
        late_spool.spool_identity = Some(id(2, 11));
        asserted_transition(&aborted, &late_spool);
        let mut intent = late_spool.clone();
        intent.sequence = 4;
        intent.spool_state = O::DeleteIntent;
        asserted_transition(&late_spool, &intent);
        let mut deleted = intent.clone();
        deleted.sequence = 5;
        deleted.spool_state = O::Deleted;
        asserted_transition(&intent, &deleted);
        let mut no_tree = deleted.clone();
        no_tree.sequence = 6;
        no_tree.tree_state = O::NeverCreated;
        asserted_transition(&deleted, &no_tree);
        let mut done = no_tree.clone();
        done.sequence = 7;
        done.phase = P::Discarded;
        asserted_transition(&no_tree, &done);

        // 両方の所有を観測済みなら、逆の削除順序も受理する。
        let before_abort = through_tree_ready()[4].clone();
        let mut cancel = before_abort.clone();
        cancel.sequence += 1;
        cancel.aborting = true;
        asserted_transition(&before_abort, &cancel);
        let mut first_delete = cancel.clone();
        first_delete.sequence += 1;
        first_delete.spool_state = O::DeleteIntent;
        asserted_transition(&cancel, &first_delete);
        let mut first_deleted = first_delete.clone();
        first_deleted.sequence += 1;
        first_deleted.spool_state = O::Deleted;
        asserted_transition(&first_delete, &first_deleted);
        let mut second_delete = first_deleted.clone();
        second_delete.sequence += 1;
        second_delete.tree_state = O::DeleteIntent;
        asserted_transition(&first_deleted, &second_delete);
        let mut second_deleted = second_delete.clone();
        second_deleted.sequence += 1;
        second_deleted.tree_state = O::Deleted;
        asserted_transition(&second_delete, &second_deleted);
        let mut finished = second_deleted.clone();
        finished.sequence += 1;
        finished.phase = P::Discarded;
        asserted_transition(&second_deleted, &finished);
    }

    #[test]
    fn abort_accepts_negative_create_observations_and_never_infers_them() {
        let a = initial(plan(0, 0, 0, 0));
        let mut abort = a.clone();
        abort.sequence = 2;
        abort.aborting = true;
        for negative in [O::CreateFailed, O::NeverCreated] {
            let mut result = abort.clone();
            result.sequence = 3;
            result.spool_state = negative;
            asserted_transition(&abort, &result);
        }
        let mut invented = abort.clone();
        invented.sequence = 3;
        invented.tree_state = O::NeverCreated;
        asserted_transition(&abort, &invented);
        let mut failed = abort.clone();
        failed.sequence = 3;
        failed.spool_state = O::CreateFailed;
        let mut both = failed.clone();
        both.sequence = 4;
        both.tree_state = O::NeverCreated;
        asserted_transition(&failed, &both);
        let mut discard = both.clone();
        discard.sequence = 5;
        discard.phase = P::Discarded;
        asserted_transition(&both, &discard);
        let mut bad = abort.clone();
        bad.sequence = 3;
        bad.spool_state = O::Deleted;
        invalid_transition(&abort, &bad);

        let spool_owned = owned_spool(&a, 2);
        let spool_ready = ready_spool(&spool_owned, 3);
        let ti = tree_intent(&spool_ready, 4);
        let mut ti_abort = ti.clone();
        ti_abort.sequence = 5;
        ti_abort.aborting = true;
        asserted_transition(&ti, &ti_abort);
        for negative in [O::CreateFailed, O::NeverCreated] {
            let mut result = ti_abort.clone();
            result.sequence = 6;
            result.tree_state = negative;
            asserted_transition(&ti_abort, &result);
        }
    }

    #[test]
    fn publish_intent_uncertainty_survives_abort_and_terminal_is_exact() {
        let mut s = through_tree_ready().pop().unwrap();
        let mut delete = s.clone();
        delete.sequence = 7;
        delete.spool_state = O::DeleteIntent;
        asserted_transition(&s, &delete);
        s = delete;
        let mut deleted = s.clone();
        deleted.sequence = 8;
        deleted.spool_state = O::Deleted;
        deleted.phase = P::SpoolCleaned;
        asserted_transition(&s, &deleted);
        s = deleted;
        let mut publish = s.clone();
        publish.sequence = 9;
        publish.phase = P::PublishIntent;
        asserted_transition(&s, &publish);
        let mut latch = publish.clone();
        latch.sequence = 10;
        latch.aborting = true;
        asserted_transition(&publish, &latch);
        let mut late = latch.clone();
        late.sequence = 11;
        late.phase = P::Published;
        late.tree_state = O::Published;
        late.publication_identity = late.tree_identity.clone();
        asserted_transition(&latch, &late);
        let mut cleanup = late.clone();
        cleanup.sequence += 1;
        cleanup.tree_state = O::DeleteIntent;
        invalid_transition(&late, &cleanup);
        let mut discard = late.clone();
        discard.sequence += 1;
        discard.phase = P::Discarded;
        invalid_transition(&late, &discard);
        let mut replay = late.clone();
        replay.sequence += 1;
        replay.publication_identity = None;
        invalid_transition(&late, &replay);
        let mut wrong_publication = late.clone();
        wrong_publication.sequence += 1;
        wrong_publication.publication_identity = Some(id(2, 99));
        invalid_transition(&late, &wrong_publication);
        let mut missing_publication = late.clone();
        missing_publication.publication_identity = None;
        assert!(missing_publication.validate().is_err());
        let mut invalid_previous = latch.clone();
        invalid_previous.spool_ready = None;
        let mut next = latch.clone();
        next.sequence += 1;
        next.phase = P::Published;
        next.tree_state = O::Published;
        next.publication_identity = next.tree_identity.clone();
        invalid_transition(&invalid_previous, &next);
    }

    #[test]
    fn every_immutable_binding_fact_is_frozen_and_observed_ids_are_real_distinct_facts() {
        let base = initial(plan(1, 2, 3, 1));
        let owned = owned_spool(&base, 2);
        let control = ready_spool(&owned, 3);
        assert!(control.validate().is_ok());
        asserted_transition(&owned, &control);
        macro_rules! binding_change_rejected {
            ($field:literal, $edit:expr, $plan_receipt:expr) => {{
                let mut candidate = control.clone();
                ($edit)(&mut candidate.binding);
                if $plan_receipt {
                    candidate.spool_ready = Some(spool_ready(&candidate.binding.plan));
                }
                assert!(
                    candidate.validate().is_ok(),
                    "{field} mutation made malformed fixture",
                    field = $field
                );
                assert!(
                    !candidate.follows(&owned),
                    "{field} mutation was accepted",
                    field = $field
                );
            }};
        }
        binding_change_rejected!(
            "attempt.run_id",
            |b: &mut JournalSevenZipExtractBinding| b.attempt.run_id[0] ^= 1,
            false
        );
        binding_change_rejected!(
            "attempt.job",
            |b: &mut JournalSevenZipExtractBinding| b.attempt.job += 1,
            false
        );
        binding_change_rejected!(
            "attempt.attempt",
            |b: &mut JournalSevenZipExtractBinding| b.attempt.attempt += 1,
            false
        );
        binding_change_rejected!(
            "item",
            |b: &mut JournalSevenZipExtractBinding| b.item += 1,
            false
        );
        binding_change_rejected!(
            "source.path",
            |b: &mut JournalSevenZipExtractBinding| b.source.path = path(r"C:\other.7z"),
            false
        );
        binding_change_rejected!(
            "source.identity.file_index",
            |b: &mut JournalSevenZipExtractBinding| b.source.identity.file_index += 1,
            false
        );
        binding_change_rejected!(
            "source.identity.volume",
            |b: &mut JournalSevenZipExtractBinding| b.source.identity.volume += 1,
            false
        );
        binding_change_rejected!(
            "source.length",
            |b: &mut JournalSevenZipExtractBinding| b.source.length += 1,
            false
        );
        binding_change_rejected!(
            "source.last_write_ticks",
            |b: &mut JournalSevenZipExtractBinding| b.source.last_write_ticks += 1,
            false
        );
        binding_change_rejected!(
            "provider_revision",
            |b: &mut JournalSevenZipExtractBinding| b.provider_revision += 1,
            false
        );
        binding_change_rejected!(
            "provider_executable.path",
            |b: &mut JournalSevenZipExtractBinding| b.provider_executable.path =
                path(r"C:\other.exe"),
            false
        );
        binding_change_rejected!(
            "provider_executable.identity",
            |b: &mut JournalSevenZipExtractBinding| b.provider_executable.identity.file_index += 1,
            false
        );
        binding_change_rejected!(
            "provider_executable.identity.volume",
            |b: &mut JournalSevenZipExtractBinding| b.provider_executable.identity.volume += 1,
            false
        );
        binding_change_rejected!(
            "provider_dll.path",
            |b: &mut JournalSevenZipExtractBinding| b.provider_dll.path = path(r"C:\other.dll"),
            false
        );
        binding_change_rejected!(
            "provider_dll.identity",
            |b: &mut JournalSevenZipExtractBinding| b.provider_dll.identity.file_index += 1,
            false
        );
        binding_change_rejected!(
            "provider_dll.identity.volume",
            |b: &mut JournalSevenZipExtractBinding| b.provider_dll.identity.volume += 1,
            false
        );
        binding_change_rejected!(
            "application_image.path",
            |b: &mut JournalSevenZipExtractBinding| b.application_image.path =
                path(r"C:\other-helper.exe"),
            false
        );
        binding_change_rejected!(
            "application_image.identity",
            |b: &mut JournalSevenZipExtractBinding| b.application_image.identity.file_index += 1,
            false
        );
        binding_change_rejected!(
            "application_image.identity.volume",
            |b: &mut JournalSevenZipExtractBinding| b.application_image.identity.volume += 1,
            false
        );
        binding_change_rejected!(
            "destination_parent.identity",
            |b: &mut JournalSevenZipExtractBinding| b.destination_parent.file_index = 99,
            false
        );
        binding_change_rejected!(
            "output.path",
            |b: &mut JournalSevenZipExtractBinding| b.output = path(r"D:\other"),
            false
        );
        binding_change_rejected!(
            "spool.path",
            |b: &mut JournalSevenZipExtractBinding| b.spool = path(r"D:\other-spool"),
            false
        );
        binding_change_rejected!(
            "tree.path",
            |b: &mut JournalSevenZipExtractBinding| b.tree = path(r"D:\other-tree"),
            false
        );
        binding_change_rejected!(
            "plan.sha256",
            |b: &mut JournalSevenZipExtractBinding| b.plan.sha256[0] ^= 1,
            true
        );
        binding_change_rejected!(
            "plan.indexed_record_count",
            |b: &mut JournalSevenZipExtractBinding| b.plan.indexed_record_count = 3,
            true
        );
        binding_change_rejected!(
            "plan.materialized_node_count",
            |b: &mut JournalSevenZipExtractBinding| b.plan.materialized_node_count = 4,
            true
        );
        binding_change_rejected!(
            "plan.file_count",
            |b: &mut JournalSevenZipExtractBinding| b.plan.file_count = 2,
            true
        );
        binding_change_rejected!(
            "plan.total_expanded_bytes",
            |b: &mut JournalSevenZipExtractBinding| b.plan.total_expanded_bytes = 2,
            true
        );
        let mut alias = owned.clone();
        alias.sequence += 1;
        alias.spool_identity = Some(id(1, 1));
        assert!(alias.validate().is_err());
        let mut same = owned_tree(&tree_intent(&ready_spool(&owned_spool(&base, 2), 3), 4), 5);
        same.tree_identity = same.spool_identity.clone();
        assert!(same.validate().is_err());
        let mut mutated = control.clone();
        mutated.spool_identity = Some(id(2, 99));
        assert!(mutated.validate().is_ok());
        assert!(
            !mutated.follows(&owned),
            "observed spool identity mutation was accepted"
        );
    }

    #[test]
    fn malformed_receipts_paths_counts_sequences_and_snapshots_are_rejected() {
        let mut s = initial(plan(0, 0, 0, 0));
        assert!(s.is_initial());
        let mut bad = s.clone();
        bad.binding.provider_revision = 0;
        assert!(!bad.is_initial());
        bad = s.clone();
        bad.spool_ready = Some(spool_ready(&bad.binding.plan));
        assert!(bad.validate().is_err());
        bad = s.clone();
        bad.binding.plan.plan_format = 2;
        assert!(bad.validate().is_err());
        let mut wrong_volume = owned_spool(&s, 2);
        wrong_volume.binding.destination_parent.volume += 1;
        assert!(wrong_volume.validate().is_err());
        bad = s.clone();
        bad.binding.plan.indexed_record_count = 2;
        assert!(bad.validate().is_err());
        bad = s.clone();
        bad.binding.plan.file_count = 1;
        assert!(bad.validate().is_err());
        bad = s.clone();
        bad.binding.plan.total_expanded_bytes = MAX_TOTAL_BYTES + 1;
        assert!(bad.validate().is_err());
        bad = s.clone();
        bad.binding.plan.indexed_record_count = MAX_PLAN_ITEMS + 1;
        bad.binding.plan.materialized_node_count = MAX_PLAN_ITEMS + 1;
        assert!(bad.validate().is_err());
        let max_catalog = initial(plan(0, MAX_PLAN_ITEMS, MAX_PLAN_ITEMS, 0));
        assert!(max_catalog.validate().is_ok());
        let max_payload = initial(plan(2, 2, 2, MAX_TOTAL_BYTES));
        assert!(max_payload.validate().is_ok());
        bad = s.clone();
        bad.binding.plan.file_count = 1;
        bad.binding.plan.indexed_record_count = 1;
        bad.binding.plan.materialized_node_count = 1;
        bad.binding.plan.total_expanded_bytes = MAX_FILE_BYTES + 1;
        assert!(bad.validate().is_err());
        let owned = owned_spool(&s, 2);
        let mut missing = ready_spool(&owned, 3);
        missing.spool_ready = None;
        assert!(missing.validate().is_err());
        let mut mismatched = ready_spool(&owned, 3);
        mismatched.spool_ready.as_mut().unwrap().length = 1;
        assert!(mismatched.validate().is_err());
        let mut inject = owned.clone();
        inject.sequence += 1;
        inject.tree_ready = Some(tree_ready(&inject.binding.plan));
        assert!(inject.validate().is_err());
        let mut tree_owned = owned_tree(&tree_intent(&ready_spool(&owned, 3), 4), 5);
        let mut ready = ready_tree(&tree_owned, 6);
        ready.tree_ready.as_mut().unwrap().file_count = 1;
        assert!(ready.validate().is_err());
        tree_owned.sequence = u64::MAX;
        let mut overflow = tree_owned.clone();
        overflow.sequence = 0;
        invalid_transition(&tree_owned, &overflow);
        let mut skipped = owned.clone();
        skipped.sequence = 4;
        skipped.phase = P::SpoolReady;
        skipped.spool_ready = Some(spool_ready(&skipped.binding.plan));
        invalid_transition(&owned, &skipped);
        let mut backward = owned.clone();
        backward.sequence = 1;
        invalid_transition(&owned, &backward);
        let mut combined = owned.clone();
        combined.sequence += 1;
        combined.phase = P::TreeCreateIntent;
        combined.tree_state = O::CreateIntent;
        invalid_transition(&owned, &combined);
        let mut changed_duplicate = owned.clone();
        changed_duplicate.spool_identity = Some(id(2, 99));
        assert!(!changed_duplicate.follows(&owned));
        let mut unack_delete = ready_tree(&tree_owned, 6);
        unack_delete.sequence = 7;
        unack_delete.spool_state = O::Deleted;
        unack_delete.phase = P::SpoolCleaned;
        invalid_transition(&ready_tree(&tree_owned, 6), &unack_delete);

        for bad_path in [
            r"C:\x\..\y",
            r"C:\x\.\y",
            r"C:\x\",
            r"C:\x.",
            r"C:\x ",
            r"C:\x:ads",
            r"\\server",
            r"\\server\",
            r"\\.\C:\x",
            r"\\?\GLOBALROOT\x",
        ] {
            let mut p = s.clone();
            p.binding.source.path = path(bad_path);
            assert!(p.validate().is_err(), "accepted {bad_path}");
        }
        let mut nul = s.clone();
        nul.binding.source.path.units.push(0);
        assert!(nul.validate().is_err());
        let mut at_limit = s.clone();
        at_limit.binding.source.path.units = [
            vec![b'C' as u16, b':' as u16, b'\\' as u16],
            vec![b'x' as u16; MAX_PATH_UNITS - 3],
        ]
        .concat();
        assert!(valid_absolute_path(&at_limit.binding.source.path));
        let mut overlong = s.clone();
        overlong.binding.source.path.units = [
            vec![b'C' as u16, b':' as u16, b'\\' as u16],
            vec![b'x' as u16; MAX_PATH_UNITS - 2],
        ]
        .concat();
        assert!(overlong.validate().is_err());
        assert!(valid_absolute_path(&path(r"\\server\share\archive.7z")));
        assert!(valid_absolute_path(&path(r"\\?\C:\folder\archive.7z")));
        assert!(valid_absolute_path(&path(
            r"\\?\UNC\server\share\archive.7z"
        )));
        let unpaired = JournalPath {
            units: vec![
                b'C' as u16,
                b':' as u16,
                b'\\' as u16,
                0xd800,
                b'.' as u16,
                b'7' as u16,
            ],
        };
        assert!(valid_absolute_path(&unpaired));
        let round =
            serde_json::from_slice::<JournalPath>(&serde_json::to_vec(&unpaired).unwrap()).unwrap();
        assert_eq!(round, unpaired);
        let mut alias = s.clone();
        alias.binding.output = path(r"D:\Ä\out");
        alias.binding.spool = path(r"D:\ä\out");
        alias.binding.tree = path(r"D:\ä\tree");
        assert!(alias.validate().is_err());
        s.sequence = 0;
        assert!(s.validate().is_err());
    }
}

fn normal_step(
    next: &mut JournalSevenZipExtractLedger,
    observed: &JournalSevenZipExtractLedger,
) -> bool {
    use JournalSevenZipExtractObjectState as O;
    use JournalSevenZipExtractPhase as P;
    match next.phase {
        P::SpoolCreateIntent => {
            next.phase = P::SpoolOwned;
            next.spool_state = O::Owned;
            next.spool_identity = observed.spool_identity.clone();
        }
        P::SpoolOwned => {
            next.phase = P::SpoolReady;
            next.spool_ready = Some(spool_receipt(&next.binding.plan));
        }
        P::SpoolReady => {
            next.phase = P::TreeCreateIntent;
            next.tree_state = O::CreateIntent;
        }
        P::TreeCreateIntent => {
            next.phase = P::TreeOwned;
            next.tree_state = O::Owned;
            next.tree_identity = observed.tree_identity.clone();
        }
        P::TreeOwned => {
            next.phase = P::TreeReady;
            next.tree_ready = Some(tree_receipt(&next.binding.plan));
        }
        P::TreeReady if next.spool_state == O::Owned => {
            next.spool_state = O::DeleteIntent;
        }
        P::TreeReady if next.spool_state == O::DeleteIntent => {
            next.phase = P::SpoolCleaned;
            next.spool_state = O::Deleted;
        }
        P::TreeReady => return false,
        P::SpoolCleaned => {
            next.phase = P::PublishIntent;
        }
        P::PublishIntent => unreachable!(),
        P::Published | P::Discarded => return false,
    }
    if next.phase == P::TreeReady && next.spool_state == O::DeleteIntent {
        return true;
    }
    if next.phase == P::SpoolCleaned && next.spool_state == O::Deleted {
        return true;
    }
    if next.phase == P::TreeReady && next.tree_ready.is_some() {
        return true;
    }
    if next.phase == P::SpoolReady && next.spool_ready.is_some() {
        return true;
    }
    if next.phase == P::TreeCreateIntent && next.tree_state == O::CreateIntent {
        return true;
    }
    if next.phase == P::Published {
        return false;
    }
    if next.phase == P::SpoolOwned && next.spool_identity.is_some() {
        return true;
    }
    if next.phase == P::TreeOwned && next.tree_identity.is_some() {
        return true;
    }
    if next.phase == P::PublishIntent {
        return true;
    }
    false
}

fn abort_step(
    next: &mut JournalSevenZipExtractLedger,
    observed: &JournalSevenZipExtractLedger,
) -> bool {
    use JournalSevenZipExtractObjectState as O;
    use JournalSevenZipExtractPhase as P;
    if is_resolved(next.spool_state) && is_resolved(next.tree_state) && next.phase != P::Discarded {
        next.phase = P::Discarded;
        return true;
    }
    // 一度に変えるのは一つの対象だけで、処理順序から削除権限を推測しない。
    for is_spool in [true, false] {
        let state = if is_spool {
            next.spool_state
        } else {
            next.tree_state
        };
        let observed_state = if is_spool {
            observed.spool_state
        } else {
            observed.tree_state
        };
        let target = match state {
            O::Planned if observed_state == O::NeverCreated => O::NeverCreated,
            O::CreateIntent if matches!(observed_state, O::CreateFailed | O::NeverCreated) => {
                observed_state
            }
            O::CreateIntent if observed_state == O::Owned => O::Owned,
            O::Owned if observed_state == O::DeleteIntent => O::DeleteIntent,
            O::DeleteIntent if observed_state == O::Deleted => O::Deleted,
            _ => continue,
        };
        if is_spool {
            next.spool_state = target;
            if target == O::Owned {
                next.spool_identity = observed.spool_identity.clone();
                next.phase = P::SpoolOwned;
            }
        } else {
            next.tree_state = target;
            if target == O::Owned {
                next.tree_identity = observed.tree_identity.clone();
                next.phase = P::TreeOwned;
            }
        }
        return true;
    }
    false
}

fn is_resolved(state: JournalSevenZipExtractObjectState) -> bool {
    matches!(
        state,
        JournalSevenZipExtractObjectState::Deleted
            | JournalSevenZipExtractObjectState::CreateFailed
            | JournalSevenZipExtractObjectState::NeverCreated
    )
}

fn valid_aborting_phase(ledger: &JournalSevenZipExtractLedger) -> bool {
    use JournalSevenZipExtractObjectState as O;
    use JournalSevenZipExtractPhase as P;
    let s = ledger.spool_state;
    let t = ledger.tree_state;
    let owned_or_cleaning = |state| matches!(state, O::Owned | O::DeleteIntent | O::Deleted);
    match ledger.phase {
        P::SpoolCreateIntent => {
            matches!(s, O::CreateIntent | O::CreateFailed | O::NeverCreated)
                && matches!(t, O::Planned | O::NeverCreated)
                && ledger.spool_ready.is_none()
                && ledger.tree_ready.is_none()
        }
        P::SpoolOwned => {
            owned_or_cleaning(s)
                && matches!(t, O::Planned | O::NeverCreated)
                && ledger.spool_ready.is_none()
                && ledger.tree_ready.is_none()
        }
        P::SpoolReady => {
            owned_or_cleaning(s)
                && matches!(t, O::Planned | O::NeverCreated)
                && ledger.spool_ready.is_some()
                && ledger.tree_ready.is_none()
        }
        P::TreeCreateIntent => {
            owned_or_cleaning(s)
                && matches!(t, O::CreateIntent | O::CreateFailed | O::NeverCreated)
                && ledger.spool_ready.is_some()
                && ledger.tree_ready.is_none()
        }
        P::TreeOwned => {
            owned_or_cleaning(s)
                && owned_or_cleaning(t)
                && ledger.spool_ready.is_some()
                && ledger.tree_ready.is_none()
        }
        P::TreeReady => {
            owned_or_cleaning(s)
                && owned_or_cleaning(t)
                && ledger.spool_ready.is_some()
                && ledger.tree_ready.is_some()
        }
        P::SpoolCleaned => s == O::Deleted && owned_or_cleaning(t) && ledger.tree_ready.is_some(),
        P::PublishIntent => {
            s == O::Deleted
                && t == O::Owned
                && ledger.tree_ready.is_some()
                && ledger.publication_identity.is_none()
        }
        P::Published => {
            s == O::Deleted
                && t == O::Published
                && ledger.tree_ready.is_some()
                && ledger.publication_identity.as_ref() == ledger.tree_identity.as_ref()
        }
        P::Discarded => {
            ledger.aborting
                && is_resolved(s)
                && is_resolved(t)
                && ledger.publication_identity.is_none()
        }
    }
}

fn spool_receipt(plan: &JournalSevenZipExtractPlan) -> JournalSevenZipExtractSpoolReady {
    JournalSevenZipExtractSpoolReady {
        plan_format: plan.plan_format,
        plan_sha256: plan.sha256,
        length: plan.total_expanded_bytes,
    }
}
fn tree_receipt(plan: &JournalSevenZipExtractPlan) -> JournalSevenZipExtractTreeReady {
    JournalSevenZipExtractTreeReady {
        plan_format: plan.plan_format,
        plan_sha256: plan.sha256,
        materialized_node_count: plan.materialized_node_count,
        file_count: plan.file_count,
        total_verified_bytes: plan.total_expanded_bytes,
    }
}
fn valid_object_identity(
    state: JournalSevenZipExtractObjectState,
    identity: bool,
    tree: bool,
) -> bool {
    use JournalSevenZipExtractObjectState as O;
    match state {
        O::Planned | O::CreateIntent | O::CreateFailed | O::NeverCreated => !identity,
        O::Owned | O::DeleteIntent | O::Deleted => identity,
        O::Published => tree && identity,
    }
}
pub(super) fn valid_absolute_path(path: &JournalPath) -> bool {
    !path.units.is_empty()
        && path.units.len() <= MAX_PATH_UNITS
        && !path.units.contains(&0)
        && path_parts(&path.units).is_some()
}
fn normalized_units(units: &[u16]) -> Vec<u16> {
    units
        .iter()
        .map(|u| if *u == b'/' as u16 { b'\\' as u16 } else { *u })
        .collect()
}
fn valid_path_component(part: &[u16]) -> bool {
    !part.is_empty()
        && part != [b'.' as u16]
        && part != [b'.' as u16, b'.' as u16]
        && !part
            .last()
            .is_some_and(|u| *u == b'.' as u16 || *u == b' ' as u16)
        && !part.contains(&(b':' as u16))
}
fn path_parts(units: &[u16]) -> Option<(Vec<u16>, Vec<Vec<u16>>)> {
    let units = normalized_units(units);
    let slash = b'\\' as u16;
    let (root_len, tail_start) = if units.len() >= 7
        && units[..4] == [slash, slash, b'?' as u16, slash]
        && matches!(units[4], 65..=90 | 97..=122)
        && units[5] == b':' as u16
        && units[6] == slash
    {
        (7, 7)
    } else if units.len() >= 8
        && paths_equal(
            &units[..8],
            &[
                slash,
                slash,
                b'?' as u16,
                slash,
                b'U' as u16,
                b'N' as u16,
                b'C' as u16,
                slash,
            ],
        )
    {
        let server_end = units[8..].iter().position(|u| *u == slash)? + 8;
        if !valid_path_component(&units[8..server_end]) {
            return None;
        }
        let share_start = server_end + 1;
        let share_end = units[share_start..]
            .iter()
            .position(|u| *u == slash)
            .map(|i| i + share_start)?;
        if !valid_path_component(&units[share_start..share_end]) {
            return None;
        }
        (share_end + 1, share_end + 1)
    } else if units.len() >= 3
        && matches!(units[0], 65..=90 | 97..=122)
        && units[1] == b':' as u16
        && units[2] == slash
    {
        (3, 3)
    } else if units.len() >= 5 && units[0] == slash && units[1] == slash && units[2] != slash {
        let server_end = units[2..].iter().position(|u| *u == slash)? + 2;
        if !valid_path_component(&units[2..server_end])
            || paths_equal(&units[2..server_end], &[b'?' as u16])
        {
            return None;
        }
        let share_start = server_end + 1;
        if share_start >= units.len() {
            return None;
        }
        let share_end = units[share_start..]
            .iter()
            .position(|u| *u == slash)
            .map(|i| i + share_start)?;
        if !valid_path_component(&units[share_start..share_end]) {
            return None;
        }
        (share_end + 1, share_end + 1)
    } else {
        return None;
    };
    if root_len >= units.len() || units.last() == Some(&slash) {
        return None;
    }
    let mut components = Vec::new();
    let tail = &units[tail_start..];
    let mut begin = 0;
    for end in 0..=tail.len() {
        if end == tail.len() || tail[end] == slash {
            if end == begin {
                return None;
            }
            let part = &tail[begin..end];
            if !valid_path_component(part) {
                return None;
            }
            components.push(part.to_vec());
            begin = end + 1;
        }
    }
    (!components.is_empty()).then_some((units[..root_len].to_vec(), components))
}
fn units_equal_ordinal(a: &[u16], b: &[u16]) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::CompareStringOrdinal;
        let mut left = a.to_vec();
        left.push(0);
        let mut right = b.to_vec();
        right.push(0);
        unsafe { CompareStringOrdinal(&left, &right, true).0 == 2 }
    }
    #[cfg(not(windows))]
    {
        fn fold(units: &[u16]) -> Vec<u16> {
            let mut out = Vec::new();
            for decoded in char::decode_utf16(units.iter().copied()) {
                match decoded {
                    Ok(ch) => {
                        for upper in ch.to_uppercase() {
                            let mut encoded = [0u16; 2];
                            out.extend_from_slice(upper.encode_utf16(&mut encoded));
                        }
                    }
                    Err(error) => out.push(error.unpaired_surrogate()),
                }
            }
            out
        }
        fold(a) == fold(b)
    }
}
pub(super) fn paths_equal(a: &[u16], b: &[u16]) -> bool {
    units_equal_ordinal(&normalized_units(a), &normalized_units(b))
}
fn same_parent(a: &JournalPath, b: &JournalPath) -> bool {
    let Some((ar, mut ac)) = path_parts(&a.units) else {
        return false;
    };
    let Some((br, mut bc)) = path_parts(&b.units) else {
        return false;
    };
    ac.pop();
    bc.pop();
    paths_equal(&ar, &br)
        && ac.len() == bc.len()
        && ac.iter().zip(&bc).all(|(x, y)| units_equal_ordinal(x, y))
}
pub(super) fn aliases(a: &JournalPath, b: &JournalPath) -> bool {
    paths_equal(&a.units, &b.units)
}
