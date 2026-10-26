use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use super::super::model::{
    FsChangeHint, ItemId, ItemOutcome, ItemReceipt, OperationKind, ProgressCounters,
    ProgressUpdate, RetryPlan, TerminalOutcome, TerminalReport,
};
use super::{AttemptToken, ExecutorFact};

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
    if request.kind != OperationKind::CreateFile
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
            report: failed("invalid create-file request"),
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
            report: failed("safe create-file operation is unsupported on this platform"),
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
    let item = ItemId(0);
    let mut counters = ProgressCounters {
        discovered_items: 1,
        total_items: Some(1),
        ..Default::default()
    };
    emit(ExecutorFact::Progress {
        token,
        update: ProgressUpdate {
            sequence: 1,
            counters,
            current_item: Some(item),
        },
    });
    let mut undo_candidate = None;
    let (outcome, receipt_outcome, detail, changed) =
        match create_candidate(parent, cancelled, |path| {
            let name = path.file_name().ok_or_else(|| std::io::Error::other("invalid create name"))?;
            match super::create_file_undo_candidate(parent, expected_identity, name) {
                Ok(receipt) => { undo_candidate = receipt; Ok(()) }
                Err(super::CopyError::DestinationExists) => Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "name collision")),
                Err(error) => Err(std::io::Error::other(format!("{error:?}"))),
            }
        }) {
            CandidateResult::Created(name) => (
                TerminalOutcome::Succeeded,
                ItemOutcome::Completed,
                Some(name),
                true,
            ),
            CandidateResult::Cancelled => (
                TerminalOutcome::Cancelled,
                ItemOutcome::CancelledBeforeStart,
                Some("cancelled before file creation".to_owned()),
                false,
            ),
            CandidateResult::Failed(error) => (
                TerminalOutcome::Failed,
                ItemOutcome::Failed,
                Some(error),
                false,
            ),
        };
    counters.processed_items = 1;
    emit(ExecutorFact::Progress {
        token,
        update: ProgressUpdate {
            sequence: 2,
            counters,
            current_item: Some(item),
        },
    });
    if outcome == TerminalOutcome::Succeeded {
        if let Some(receipt) = undo_candidate {
            emit(ExecutorFact::CreatedObjectUndoCandidate { token, item, receipt });
        }
    }
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: counters,
            receipts: vec![ItemReceipt {
                item,
                outcome: receipt_outcome,
                detail,
                created_folder_path: None,
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

enum CandidateResult {
    Created(String),
    Cancelled,
    Failed(String),
}

fn create_candidate(
    parent: &Path,
    cancelled: &AtomicBool,
    mut create_new: impl FnMut(&Path) -> std::io::Result<()>,
) -> CandidateResult {
    for suffix in 1..=999 {
        if cancelled.load(Ordering::Acquire) {
            return CandidateResult::Cancelled;
        }
        let name = if suffix == 1 {
            "New File.txt".to_owned()
        } else {
            format!("New File ({suffix}).txt")
        };
        match create_new(&parent.join(&name)) {
            Ok(()) => return CandidateResult::Created(name),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return CandidateResult::Failed(error.to_string()),
        }
    }
    CandidateResult::Failed("all file names through New File (999).txt already exist".to_owned())
}

fn failed(detail: &str) -> TerminalReport {
    TerminalReport {
        outcome: TerminalOutcome::Failed,
        final_counters: ProgressCounters {
            discovered_items: 1,
            total_items: Some(1),
            ..Default::default()
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

#[cfg(test)]
mod tests {
    use super::{CandidateResult, create_candidate};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn create_file_uses_empty_create_new_candidates_without_overwriting() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::write(parent.path().join("New File.txt"), b"sentinel").unwrap();
        let result = create_candidate(parent.path(), &AtomicBool::new(false), |path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(drop)
        });
        assert!(matches!(result, CandidateResult::Created(ref name) if name == "New File (2).txt"));
        assert_eq!(
            std::fs::read(parent.path().join("New File.txt")).unwrap(),
            b"sentinel"
        );
        assert_eq!(
            std::fs::metadata(parent.path().join("New File (2).txt"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn create_file_stops_on_non_collision_error_and_cancellation() {
        let parent = tempfile::tempdir().unwrap();
        let failure = create_candidate(parent.path(), &AtomicBool::new(false), |_| {
            Err(std::io::Error::other("sentinel failure"))
        });
        assert!(
            matches!(failure, CandidateResult::Failed(message) if message == "sentinel failure")
        );
        assert!(matches!(
            create_candidate(parent.path(), &AtomicBool::new(true), |_| panic!(
                "cancelled before create"
            )),
            CandidateResult::Cancelled
        ));
    }

    #[test]
    fn create_file_keeps_atomic_success_when_cancellation_arrives_after_create() {
        let parent = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let result = create_candidate(parent.path(), &cancelled, |path| {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            drop(file);
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        });
        assert!(matches!(result, CandidateResult::Created(ref name) if name == "New File.txt"));
        assert_eq!(
            std::fs::metadata(parent.path().join("New File.txt"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn create_file_reports_exhausted_names_after_collision_only() {
        let parent = tempfile::tempdir().unwrap();
        let mut attempts = 0;
        let result = create_candidate(parent.path(), &AtomicBool::new(false), |_| {
            attempts += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "occupied",
            ))
        });
        assert_eq!(attempts, 999);
        assert!(
            matches!(result, CandidateResult::Failed(message) if message.contains("New File (999).txt"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn created_file_undo_removes_exact_unchanged_empty_file() {
        let parent = tempfile::tempdir().unwrap();
        let guard = super::super::directory_identity_for_test(parent.path());
        let receipt = super::super::create_file_undo_candidate(parent.path(), &guard, std::ffi::OsStr::new("New File.txt"))
            .unwrap().expect("same-handle file proof");
        let destination = receipt.destination.clone();
        super::super::undo_created_object(receipt).unwrap();
        assert!(!destination.exists());
    }

    #[cfg(windows)]
    #[test]
    fn created_file_undo_refuses_edit_and_retains_bytes() {
        let parent = tempfile::tempdir().unwrap();
        let guard = super::super::directory_identity_for_test(parent.path());
        let receipt = super::super::create_file_undo_candidate(parent.path(), &guard, std::ffi::OsStr::new("New File.txt"))
            .unwrap().expect("same-handle file proof");
        std::fs::write(&receipt.destination, b"edited").unwrap();
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert_eq!(std::fs::read(receipt.destination).unwrap(), b"edited");
    }

    #[cfg(windows)]
    #[test]
    fn created_file_undo_refuses_replaced_leaf_and_parent() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let guard = super::super::directory_identity_for_test(&parent);
        let receipt = super::super::create_file_undo_candidate(&parent, &guard, std::ffi::OsStr::new("New File.txt"))
            .unwrap().expect("same-handle file proof");
        let moved = root.path().join("original file");
        std::fs::rename(&receipt.destination, &moved).unwrap();
        std::fs::write(&receipt.destination, b"replacement").unwrap();
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert_eq!(std::fs::read(&receipt.destination).unwrap(), b"replacement");
        assert_eq!(std::fs::metadata(moved).unwrap().len(), 0);

        let receipt = super::super::create_file_undo_candidate(&parent, &guard, std::ffi::OsStr::new("Second.txt"))
            .unwrap().expect("second file proof");
        let old_parent = root.path().join("old parent");
        std::fs::rename(&parent, &old_parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert_eq!(std::fs::metadata(old_parent.join("Second.txt")).unwrap().len(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn created_file_undo_refuses_hardlink_and_reparse_replacements() {
        let parent = tempfile::tempdir().unwrap();
        let guard = super::super::directory_identity_for_test(parent.path());
        let receipt = super::super::create_file_undo_candidate(parent.path(), &guard, std::ffi::OsStr::new("New File.txt"))
            .unwrap().expect("same-handle file proof");
        let hardlink = parent.path().join("hardlink.txt");
        std::fs::hard_link(&receipt.destination, &hardlink).unwrap();
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert!(hardlink.exists());
        std::fs::remove_file(hardlink).unwrap();

        let receipt = super::super::create_file_undo_candidate(parent.path(), &guard, std::ffi::OsStr::new("Reparse.txt"))
            .unwrap().expect("second same-handle file proof");
        let sentinel = parent.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"safe").unwrap();
        std::fs::remove_file(&receipt.destination).unwrap();
        if std::os::windows::fs::symlink_file(&sentinel, &receipt.destination).is_err() {
            return;
        }
        assert!(super::super::undo_created_object(receipt.clone()).is_err());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"safe");
        assert!(std::fs::symlink_metadata(receipt.destination).unwrap().file_type().is_symlink());
    }
}
