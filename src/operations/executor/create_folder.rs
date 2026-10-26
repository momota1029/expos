use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::super::model::{
    AttemptToken, FsChangeHint, ItemId, ItemOutcome, ItemReceipt, OperationKind, ProgressCounters,
    ProgressUpdate, RetryPlan, TerminalOutcome, TerminalReport,
};
use super::ExecutorFact;

pub(super) fn execute(
    token: AttemptToken,
    request: &super::super::model::OperationRequest,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let Some(parent) = request.destination.as_deref() else {
        emit(ExecutorFact::Terminal {
            token,
            report: failed("missing parent directory"),
        });
        return;
    };
    let Some(expected_identity) = request.create_parent_directory_identity_guard.as_ref() else {
        emit(ExecutorFact::Terminal {
            token,
            report: failed("missing parent identity"),
        });
        return;
    };
    if request.kind != OperationKind::CreateFolder
        || !request.sources.is_empty()
        || request.virtual_source.is_some()
        || request.batch_rename_plan.is_some()
        || request.move_identity_guard.is_some()
        || request.rename_reparse_guard.is_some()
        || request.move_destination_identity_guard.is_some()
        || request.source_cleanup_identity_guard.is_some()
        || request.copy_destination_identity_guard.is_some()
    {
        emit(ExecutorFact::Terminal {
            token,
            report: failed("invalid create-folder request"),
        });
        return;
    }

    #[cfg(windows)]
    let _destination_lease =
        match super::super::rename::pin_destination_directory_identity(parent, expected_identity) {
            Ok(lease) => lease,
            Err(error) => {
                emit(ExecutorFact::Terminal {
                    token,
                    report: failed(&format!("parent directory identity changed: {error:?}")),
                });
                return;
            }
        };

    #[cfg(not(windows))]
    {
        let _ = expected_identity;
        emit(ExecutorFact::Terminal {
            token,
            report: failed("safe create-folder operation is unsupported on this platform"),
        });
        return;
    }

    #[cfg(windows)]
    execute_candidates(token, parent, expected_identity, cancelled, emit);
}

#[cfg(windows)]
fn execute_candidates(
    token: AttemptToken,
    parent: &Path,
    expected_identity: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
) {
    execute_candidates_with_proof(token, parent, cancelled, emit, |path| {
        let name = path.file_name().ok_or_else(|| std::io::Error::other("invalid create name"))?;
        match super::create_folder_undo_candidate(parent, expected_identity, name) {
            Ok(receipt) => Ok(receipt),
            Err(super::CopyError::DestinationExists) => Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "name collision")),
            Err(error) => Err(std::io::Error::other(format!("{error:?}"))),
        }
    });
}

#[cfg(windows)]
fn execute_candidates_with(
    token: AttemptToken,
    parent: &Path,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
    mut create_dir: impl FnMut(&Path) -> std::io::Result<()>,
) {
    execute_candidates_with_proof(token, parent, cancelled, emit, |path| create_dir(path).map(|()| None));
}

#[cfg(windows)]
fn execute_candidates_with_proof(
    token: AttemptToken,
    parent: &Path,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
    mut create_dir: impl FnMut(&Path) -> std::io::Result<Option<super::super::model::CreatedObjectUndoReceipt>>,
) {
    let item = ItemId(0);
    let mut counters = ProgressCounters {
        discovered_items: 1,
        total_items: Some(1),
        ..ProgressCounters::default()
    };
    emit(ExecutorFact::Progress {
        token,
        update: ProgressUpdate {
            sequence: 1,
            counters,
            current_item: Some(item),
        },
    });
    let mut created = None;
    let mut undo_candidate = None;
    let mut failure = None;
    let mut cancelled_before_create = false;

    for suffix in 1..=999 {
        if cancelled.load(Ordering::Acquire) {
            cancelled_before_create = true;
            break;
        }
        let name = if suffix == 1 {
            "New Folder".to_owned()
        } else {
            format!("New Folder ({suffix})")
        };
        let candidate = parent.join(name);
        match create_dir(&candidate) {
            Ok(proof) => {
                created = Some(candidate);
                undo_candidate = proof;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
    }
    if created.is_none() && failure.is_none() && !cancelled_before_create {
        failure = Some("all folder names through New Folder (999) already exist".to_owned());
    }

    let created_folder_path = created.clone();
    let (outcome, receipt_outcome, detail, changed) = if let Some(path) = created {
        (
            TerminalOutcome::Succeeded,
            ItemOutcome::Completed,
            Some(path.to_string_lossy().into_owned()),
            true,
        )
    } else if cancelled_before_create {
        (
            TerminalOutcome::Cancelled,
            ItemOutcome::CancelledBeforeStart,
            Some("cancelled before folder creation".to_owned()),
            false,
        )
    } else {
        (TerminalOutcome::Failed, ItemOutcome::Failed, failure, false)
    };
    if created_folder_path.is_some() {
        if let Some(receipt) = undo_candidate {
            emit(ExecutorFact::CreatedObjectUndoCandidate { token, item, receipt });
        }
    }
    counters.processed_items = 1;
    emit(ExecutorFact::Progress {
        token,
        update: ProgressUpdate {
            sequence: 2,
            counters,
            current_item: Some(item),
        },
    });
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: counters,
            receipts: vec![ItemReceipt {
                item,
                outcome: receipt_outcome,
                detail,
                created_folder_path,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: if changed {
                vec![FsChangeHint {
                    directory: parent.to_path_buf(),
                }]
            } else {
                Vec::new()
            },
        },
    });
}

fn failed(detail: &str) -> TerminalReport {
    TerminalReport {
        outcome: TerminalOutcome::Failed,
        final_counters: ProgressCounters {
            discovered_items: 1,
            total_items: Some(1),
            ..ProgressCounters::default()
        },
        receipts: vec![ItemReceipt {
            item: ItemId(0),
            outcome: ItemOutcome::Failed,
            detail: Some(detail.to_owned()),
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::directory::{EntryKind, EntrySourceIdentity};
    use crate::operations::model::{ExecutorCommand, OperationRequest, OperationService};
    use std::sync::atomic::AtomicBool;

    fn request(parent: &Path) -> OperationRequest {
        let identity = EntrySourceIdentity::from_path(parent, EntryKind::Folder)
            .expect("directory identity")
            .filesystem_identity();
        OperationRequest {
            kind: OperationKind::CreateFolder,
            virtual_source: None,
            sources: Vec::new(),
            destination: Some(parent.to_path_buf()),
            move_identity_guard: None,
            clipboard_move_source_evidence: None,
            rename_reparse_guard: None,
            move_destination_identity_guard: None,
            move_replace_source_evidence: None,
            trash_source_guards: None,
            source_cleanup_identity_guard: None,
            initial_conflict_policy: None,
            copy_destination_identity_guard: None,
            create_parent_directory_identity_guard: Some(identity.clone()),
            zip_create_guard: None,
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
            batch_rename_plan: None,
        }
    }

    fn run(parent: &Path, cancelled: &AtomicBool) -> Vec<ExecutorFact> {
        let request = request(parent);
        let mut service = OperationService::default();
        let token = service.submit(request.clone()).expect("attempt token");
        let mut facts = Vec::new();
        execute(token, &request, cancelled, &mut |fact| facts.push(fact));
        facts
    }

    fn terminal(facts: &[ExecutorFact]) -> &TerminalReport {
        facts
            .iter()
            .find_map(|fact| match fact {
                ExecutorFact::Terminal { report, .. } => Some(report),
                _ => None,
            })
            .expect("terminal fact")
    }

    #[test]
    fn new_folder_uses_default_then_skips_file_and_broken_link_collisions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("New Folder"), b"file collision").unwrap();
        let junction_target = root.path().join("junction-target");
        let broken_junction = root.path().join("New Folder (2)");
        std::fs::create_dir(&junction_target).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&broken_junction)
            .arg(&junction_target)
            .status()
            .expect("run mklink /J for broken-junction collision fixture");
        assert!(
            status.success(),
            "mklink /J must create the fixture junction"
        );
        std::fs::remove_dir(&junction_target).unwrap();
        let facts = run(root.path(), &AtomicBool::new(false));
        let report = terminal(&facts);
        assert_eq!(report.outcome, TerminalOutcome::Succeeded);
        let receipt = &report.receipts[0];
        let expected_name = "New Folder (3)";
        let created = root.path().join(expected_name);
        assert_eq!(
            receipt.created_folder_path.as_deref(),
            Some(created.as_path())
        );
        assert!(created.is_dir());
        std::fs::remove_dir(&broken_junction).unwrap();
        assert_eq!(report.retry, RetryPlan::NotAllowed);
        assert_eq!(
            report.change_hints,
            vec![FsChangeHint {
                directory: root.path().to_path_buf()
            }]
        );
        assert!(
            matches!(facts.first(), Some(ExecutorFact::Progress { update, .. }) if update.counters.total_items == Some(1))
        );
    }

    #[test]
    fn new_folder_uses_default_name_when_available() {
        let root = tempfile::tempdir().unwrap();
        let facts = run(root.path(), &AtomicBool::new(false));
        let report = terminal(&facts);
        let expected = root.path().join("New Folder");
        assert_eq!(
            report.receipts[0].created_folder_path.as_deref(),
            Some(expected.as_path())
        );
    }

    #[test]
    fn new_folder_stops_at_the_last_supported_suffix() {
        let root = tempfile::tempdir().unwrap();
        for suffix in 1..=999 {
            let name = if suffix == 1 {
                "New Folder".to_owned()
            } else {
                format!("New Folder ({suffix})")
            };
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let facts = run(root.path(), &AtomicBool::new(false));
        let report = terminal(&facts);
        assert_eq!(report.outcome, TerminalOutcome::Failed);
        assert_eq!(report.retry, RetryPlan::NotAllowed);
        assert!(report.change_hints.is_empty());
    }

    #[test]
    fn new_folder_cancellation_before_first_candidate_is_harmless() {
        let root = tempfile::tempdir().unwrap();
        let facts = run(root.path(), &AtomicBool::new(true));
        let report = terminal(&facts);
        assert_eq!(report.outcome, TerminalOutcome::Cancelled);
        assert_eq!(
            report.receipts[0].outcome,
            ItemOutcome::CancelledBeforeStart
        );
        assert!(!root.path().join("New Folder").exists());
    }

    #[test]
    fn new_folder_keeps_success_when_cancel_arrives_after_create() {
        let root = tempfile::tempdir().unwrap();
        let request = request(root.path());
        let mut service = OperationService::default();
        let token = service.submit(request.clone()).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut facts = Vec::new();
        execute_candidates_with(
            token,
            root.path(),
            &cancelled,
            &mut |fact| facts.push(fact),
            |path| {
                std::fs::create_dir(path)?;
                cancelled.store(true, Ordering::Release);
                Ok(())
            },
        );
        let report = terminal(&facts);
        assert_eq!(report.outcome, TerminalOutcome::Succeeded);
        assert!(root.path().join("New Folder").is_dir());
        assert_eq!(report.receipts[0].outcome, ItemOutcome::Completed);
    }

    #[test]
    fn new_folder_receipt_path_round_trips_through_inspection_journal() {
        let root = tempfile::tempdir().unwrap();
        let request = request(root.path());
        let mut service = OperationService::default();
        let submitted = service.submit(request).unwrap();
        let Some(ExecutorCommand::Prepare { token, request, .. }) = service.start_next() else {
            panic!("submitted operation should be queued")
        };
        assert_eq!(token, submitted);
        assert!(service.prepared(token));
        let mut facts = Vec::new();
        execute(token, &request, &AtomicBool::new(false), &mut |fact| {
            facts.push(fact)
        });
        assert!(facts.iter().any(|fact| matches!(fact, ExecutorFact::Progress { update, .. } if update.current_item == Some(ItemId(0)) && update.counters.total_items == Some(1))));
        let report = terminal(&facts).clone();
        assert_eq!(report.receipts.len(), 1);
        assert_eq!(report.retry, RetryPlan::NotAllowed);
        assert!(service.terminal(token, report).is_some());
        let job = service.job(token.job_id()).unwrap();
        let journal = crate::operations::journal::attempt_from_snapshot(&job, &job.attempts[0]);
        let expected = root.path().join("New Folder");
        assert_eq!(
            journal.created_folder_path,
            Some(crate::operations::journal::JournalPath::from_path(
                &expected
            ))
        );
        let encoded = serde_json::to_vec(&journal).unwrap();
        let decoded: crate::operations::journal::JournalAttempt =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.created_folder_path, journal.created_folder_path);
    }

    #[test]
    fn new_folder_rejects_replaced_parent_identity() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("ancestor");
        let parent = ancestor.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let request = request(&parent);
        let old = root.path().join("old-parent");
        std::fs::rename(&parent, &old).unwrap();
        std::fs::create_dir(&parent).unwrap();
        let mut service = OperationService::default();
        let token = service.submit(request.clone()).unwrap();
        let mut facts = Vec::new();
        execute(token, &request, &AtomicBool::new(false), &mut |fact| {
            facts.push(fact)
        });
        assert_eq!(terminal(&facts).outcome, TerminalOutcome::Failed);
        assert!(!parent.join("New Folder").exists());
    }

    #[test]
    fn new_folder_holds_parent_against_rename_and_delete() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join("ancestor");
        let parent = ancestor.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let identity = EntrySourceIdentity::from_path(&parent, EntryKind::Folder)
            .unwrap()
            .filesystem_identity();
        let lease =
            super::super::super::rename::pin_destination_directory_identity(&parent, &identity)
                .expect("pin parent identity");
        assert!(std::fs::remove_dir(&parent).is_err());
        assert!(std::fs::rename(&parent, ancestor.join("parent-moved")).is_err());
        assert!(std::fs::rename(&ancestor, root.path().join("renamed")).is_err());
        drop(lease);
        std::fs::remove_dir(&parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::rename(&parent, ancestor.join("parent-moved")).unwrap();
        std::fs::rename(&ancestor, root.path().join("renamed")).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn created_folder_undo_removes_only_the_exact_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path();
        let guard = super::super::directory_identity_for_test(parent);
        let receipt = super::super::create_folder_undo_candidate(parent, &guard, std::ffi::OsStr::new("New Folder"))
            .unwrap().expect("same-handle folder proof");
        let destination = receipt.destination.clone();
        super::super::undo_created_object(receipt).unwrap();
        assert!(!destination.exists());
    }

    #[cfg(windows)]
    #[test]
    fn created_folder_undo_refuses_populated_folder_and_keeps_receipt_retryable() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path();
        let guard = super::super::directory_identity_for_test(parent);
        let receipt = super::super::create_folder_undo_candidate(parent, &guard, std::ffi::OsStr::new("New Folder"))
            .unwrap().expect("same-handle folder proof");
        let child = receipt.destination.join("sentinel");
        std::fs::write(&child, b"keep").unwrap();
        assert!(super::super::undo_created_object(receipt).is_err());
        assert_eq!(std::fs::read(child).unwrap(), b"keep");
    }

    #[cfg(windows)]
    #[test]
    fn created_folder_undo_refuses_replaced_leaf_and_parent() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let guard = super::super::directory_identity_for_test(&parent);
        let receipt = super::super::create_folder_undo_candidate(&parent, &guard, std::ffi::OsStr::new("New Folder"))
            .unwrap().expect("same-handle folder proof");
        let moved = root.path().join("original folder");
        std::fs::rename(&receipt.destination, &moved).unwrap();
        std::fs::create_dir(&receipt.destination).unwrap();
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert!(moved.is_dir() && receipt.destination.is_dir());

        let receipt = super::super::create_folder_undo_candidate(&parent, &guard, std::ffi::OsStr::new("Second"))
            .unwrap().expect("second folder proof");
        let old_parent = root.path().join("old parent");
        std::fs::rename(&parent, &old_parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert!(old_parent.join("Second").is_dir());
        assert!(parent.is_dir());
    }
}
