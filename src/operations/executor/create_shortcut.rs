use std::sync::atomic::AtomicBool;

use super::ExecutorFact;
use crate::operations::model::{
    AttemptToken, FsChangeHint, ItemId, ItemOutcome, ItemReceipt, OperationKind, OperationRequest,
    ProgressCounters, ProgressUpdate, RetryPlan, TerminalOutcome, TerminalReport,
};

pub(super) enum CreateShortcutResult {
    Created {
        destination: std::path::PathBuf,
        size: u64,
        undo: Option<crate::operations::model::CreatedObjectUndoReceipt>,
    },
    Cancelled,
    Failed(super::CopyError),
    Retained {
        destination: std::path::PathBuf,
        error: super::CopyError,
    },
}

pub(super) fn execute(
    token: AttemptToken,
    request: &OperationRequest,
    cancelled: &AtomicBool,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let Some(guard) = request.shortcut_create_guard.as_ref() else {
        emit_terminal(token, failed("missing captured shortcut identities"), emit);
        return;
    };
    if request.kind != OperationKind::CreateShortcut || !request.shortcut_create_shape_is_valid() {
        emit_terminal(token, failed("invalid shortcut request"), emit);
        return;
    }
    let source = &request.sources[0];
    let Some(expected_destination) = request.shortcut_output_path() else {
        emit_terminal(token, failed("invalid shortcut leaf name"), emit);
        return;
    };
    emit(ExecutorFact::Progress {
        token,
        update: ProgressUpdate {
            sequence: 1,
            counters: ProgressCounters {
                discovered_items: 1,
                total_items: Some(1),
                ..Default::default()
            },
            current_item: Some(ItemId(0)),
        },
    });

    #[cfg(windows)]
    let result = super::windows::shortcut::create_shortcut(
        source,
        guard.source_kind,
        &guard.source_identity,
        &guard.parent_identity,
        cancelled,
    );
    #[cfg(not(windows))]
    let result = {
        let _ = (source, guard, cancelled);
        CreateShortcutResult::Failed(super::CopyError::Unsupported)
    };

    let item = ItemId(0);
    let (outcome, item_outcome, detail, processed_bytes, hint, undo) = match result {
        CreateShortcutResult::Created {
            destination,
            size,
            undo,
        } if destination == expected_destination => (
            TerminalOutcome::Succeeded,
            ItemOutcome::Completed,
            Some(destination.display().to_string()),
            size,
            true,
            undo,
        ),
        CreateShortcutResult::Created { destination, .. } => (
            TerminalOutcome::Unknown,
            ItemOutcome::Unknown,
            Some(format!(
                "created shortcut path did not match request: {}",
                destination.display()
            )),
            0,
            true,
            None,
        ),
        CreateShortcutResult::Cancelled => (
            TerminalOutcome::Cancelled,
            ItemOutcome::CancelledBeforeStart,
            Some("cancelled before shortcut creation".into()),
            0,
            false,
            None,
        ),
        CreateShortcutResult::Failed(error) => (
            TerminalOutcome::Failed,
            ItemOutcome::Failed,
            Some(format!("{error:?}")),
            0,
            true,
            None,
        ),
        CreateShortcutResult::Retained { destination, error } => (
            TerminalOutcome::Unknown,
            ItemOutcome::Unknown,
            Some(format!(
                "shortcut output may remain at {}: {error:?}",
                destination.display()
            )),
            0,
            true,
            None,
        ),
    };
    let counters = ProgressCounters {
        discovered_items: 1,
        total_items: Some(1),
        processed_items: if item_outcome == ItemOutcome::CancelledBeforeStart {
            0
        } else {
            1
        },
        processed_bytes,
        ..Default::default()
    };
    emit(ExecutorFact::Progress {
        token,
        update: ProgressUpdate {
            sequence: 2,
            counters,
            current_item: Some(item),
        },
    });
    if let Some(receipt) = undo.filter(|_| outcome == TerminalOutcome::Succeeded) {
        emit(ExecutorFact::CreatedObjectUndoCandidate {
            token,
            item,
            receipt,
        });
    }
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome,
            final_counters: counters,
            receipts: vec![ItemReceipt {
                item,
                outcome: item_outcome,
                detail,
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: if outcome == TerminalOutcome::Unknown {
                RetryPlan::RequiresReinspection
            } else {
                RetryPlan::NotAllowed
            },
            change_hints: if hint {
                vec![FsChangeHint {
                    directory: request
                        .destination
                        .as_ref()
                        .expect("validated parent")
                        .clone(),
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
        final_counters: ProgressCounters::default(),
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

fn emit_terminal(token: AttemptToken, report: TerminalReport, emit: &mut impl FnMut(ExecutorFact)) {
    emit(ExecutorFact::Terminal { token, report });
}
