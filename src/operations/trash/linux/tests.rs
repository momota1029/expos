use std::{os::unix::fs::symlink, process::Command};

use super::*;
use crate::operations::{
    executor::{ExecutorFact, execute_prepare},
    model::{
        AttemptToken, ItemOutcome, OperationKind, OperationRequest, RetryError, RetryPlan,
        TerminalOutcome, TerminalReport, TrashSourceGuard,
    },
    runtime::{OperationRuntime, UndoEntry},
    trash,
};

#[test]
fn linux_trash_round_trip_and_uncertain_receipt() {
    const FIXTURE_ENV: &str = "EXPOS_TEST_LINUX_TRASH_FIXTURE";
    if let Some(root) = std::env::var_os(FIXTURE_ENV) {
        let root = PathBuf::from(root);
        assert_eq!(std::env::var_os("HOME").as_deref(), Some(root.as_os_str()));
        assert_eq!(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            Some(root.as_os_str())
        );
        check_trash_scenarios(&root);
        return;
    }

    // 子プロセスでだけ HOME と XDG_DATA_HOME を差し替え、並行テストと実環境を隔離する。
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let module = module_path!().split_once("::").unwrap().1;
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{module}::linux_trash_round_trip_and_uncertain_receipt"),
            "--nocapture",
        ])
        .env(FIXTURE_ENV, &root)
        .env("HOME", &root)
        .env("XDG_DATA_HOME", &root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_trash(
    runtime: &mut OperationRuntime,
    path: &Path,
    kind: EntryKind,
) -> (AttemptToken, TerminalReport, usize) {
    let identity = EntrySourceIdentity::from_path_no_follow(path, kind).unwrap();
    let token = runtime
        .submit(OperationRequest {
            kind: OperationKind::Trash,
            virtual_source: None,
            sources: vec![path.to_path_buf()],
            trash_source_guards: Some(vec![TrashSourceGuard {
                identity,
                reparse_candidate: false,
            }]),
            destination: None,
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
            shortcut_create_guard: None,
            seven_zip_create_guard: None,
            seven_zip_extract_guard: None,
            origin: None,
        })
        .unwrap();
    let worker = runtime.start_next().unwrap();
    let mut terminal = None;
    let mut receipts = 0;
    execute_prepare(
        worker.command,
        &worker.cancellation,
        &worker.controls,
        |fact| {
            let is_terminal = matches!(&fact, ExecutorFact::Terminal { .. });
            match &fact {
                ExecutorFact::TrashReceipt { .. } => receipts += 1,
                ExecutorFact::Terminal { report, .. } => terminal = Some(report.clone()),
                _ => {}
            }
            assert_eq!(runtime.commit_fact(fact).is_some(), is_terminal);
        },
    );
    runtime.mark_operation_worker_returned(token);
    (token, terminal.unwrap(), receipts)
}

fn check_trash_scenarios(root: &Path) {
    let mut runtime = OperationRuntime::default();
    let source = root.join("report 100%.txt");
    fs::write(&source, b"original").unwrap();
    let (_, report, receipts) = run_trash(&mut runtime, &source, EntryKind::File);
    assert_eq!(report.outcome, TerminalOutcome::Succeeded);
    assert_eq!(receipts, 1);
    assert!(!source.exists());
    let (reservation, UndoEntry::Trash(receipt)) = runtime.reserve_undo().unwrap() else {
        panic!("successful Trash must reserve its exact Undo receipt");
    };
    let stale_receipt = receipt.clone();
    assert_eq!(receipt.original_path(), source);
    assert!(receipt.item.time_deleted > 0);
    let (info, payload) = item_paths(&receipt.item).unwrap();
    assert_eq!(payload.parent().unwrap(), root.join("Trash/files"));
    assert_eq!(payload.file_name().unwrap(), source.file_name().unwrap());
    assert_eq!(fs::read(&payload).unwrap(), b"original");

    symlink(root.join("missing"), &source).unwrap();
    assert_eq!(
        trash::restore(receipt.clone()),
        Err(TrashError::RestoreCollision(source.clone()))
    );
    assert!(fs::symlink_metadata(&source).unwrap().is_symlink());
    fs::remove_file(&source).unwrap();
    fs::write(&source, b"late collision").unwrap();
    // 共有 preflight を通さず、preflight 後に現れた衝突を rename 自体が拒否することを確認する。
    assert_eq!(
        super::restore(receipt.clone()),
        Err(TrashError::RestoreCollision(source.clone()))
    );
    assert_eq!(fs::read(&source).unwrap(), b"late collision");
    assert_eq!(fs::read(&payload).unwrap(), b"original");
    assert!(info.is_file());
    assert!(runtime.finish_trash_undo(reservation, false));
    fs::remove_file(&source).unwrap();
    let (reservation, UndoEntry::Trash(kept)) = runtime.reserve_undo().unwrap() else {
        panic!("collision must keep the receipt for a later Undo");
    };
    assert_eq!(trash::restore(kept), Ok(RestoreDisposition::Completed));
    assert!(runtime.finish_trash_undo(reservation, true));
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert!(!info.exists());
    assert!(!payload.exists());
    assert!(runtime.reserve_undo().is_none());

    let folder = root.join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("child"), b"inside").unwrap();
    let (_, report, receipts) = run_trash(&mut runtime, &folder, EntryKind::Folder);
    assert_eq!(report.outcome, TerminalOutcome::Succeeded);
    assert_eq!(receipts, 1);
    let (reservation, UndoEntry::Trash(receipt)) = runtime.reserve_undo().unwrap() else {
        panic!("folder Trash must provide an Undo receipt");
    };
    assert_eq!(trash::restore(receipt), Ok(RestoreDisposition::Completed));
    assert!(runtime.finish_trash_undo(reservation, true));
    assert_eq!(fs::read(folder.join("child")).unwrap(), b"inside");

    fs::rename(&source, root.join("restored-sibling")).unwrap();
    fs::write(&source, b"original").unwrap();
    ::trash::delete(&source).unwrap();
    assert!(matches!(
        trash::restore(stale_receipt),
        Err(TrashError::Platform(_))
    ));
    assert!(!source.exists());
    fs::write(&source, b"second object").unwrap();
    let (token, report, receipts) = run_trash(&mut runtime, &source, EntryKind::File);
    assert_eq!(report.outcome, TerminalOutcome::Unknown);
    assert_eq!(report.receipts[0].outcome, ItemOutcome::Unknown);
    assert!(
        report.receipts[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("Multiple Trash entries")
    );
    assert_eq!(report.change_hints[0].directory, root);
    assert_eq!(report.retry, RetryPlan::RequiresReinspection);
    assert_eq!(receipts, 0);
    assert!(runtime.reserve_undo().is_none());
    assert!(!source.exists());
    fs::write(&source, b"new occupant").unwrap();
    assert_eq!(runtime.retry(token), Err(RetryError::NotRetryable));
    assert!(runtime.start_next().is_none());
    assert_eq!(fs::read(&source).unwrap(), b"new occupant");
    let mut payloads: Vec<_> = list()
        .unwrap()
        .iter()
        .filter(|item| item.original_path() == source)
        .map(|item| fs::read(item_paths(item).unwrap().1).unwrap())
        .collect();
    payloads.sort();
    assert_eq!(payloads, [b"original".to_vec(), b"second object".to_vec()]);
}
