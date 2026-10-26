use super::*;

#[cfg(windows)]
#[test]
fn queued_symbolic_link_rejects_a_replaced_destination_parent() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.txt");
    let destination = root.path().join("destination");
    let displaced = root.path().join("destination-captured");
    std::fs::write(&source, b"contents").unwrap();
    std::fs::create_dir(&destination).unwrap();
    let captured_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
        &destination,
        crate::directory::EntryKind::Folder,
    )
    .unwrap()
    .filesystem_identity();
    let request = OperationRequest {
        kind: OperationKind::CreateSymbolicLink,
        virtual_source: None,
        sources: vec![source],
        destination: Some(destination.clone()),
        move_identity_guard: None,
        clipboard_move_source_evidence: None,
        rename_reparse_guard: None,
        move_destination_identity_guard: None,
        move_replace_source_evidence: None,
        trash_source_guards: None,
        source_cleanup_identity_guard: None,
        initial_conflict_policy: None,
        copy_destination_identity_guard: None,
        create_parent_directory_identity_guard: Some(captured_identity.clone()),
        zip_create_guard: None,
        shortcut_create_guard: None,
        seven_zip_create_guard: None,
        seven_zip_extract_guard: None,
        origin: Some(crate::operations::model::OperationOrigin {
            view_id: 1,
            location_epoch: 1,
            presentation_revision: 1,
        }),
        batch_rename_plan: None,
    };
    let mut runtime = crate::operations::runtime::OperationRuntime::default();
    let token = runtime
        .submit(request)
        .expect("captured link request is admitted");
    let start = runtime.start_next().expect("queued link request starts");

    std::fs::rename(&destination, &displaced).unwrap();
    std::fs::create_dir(&destination).unwrap();
    let replacement_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
        &destination,
        crate::directory::EntryKind::Folder,
    )
    .unwrap()
    .filesystem_identity();
    assert_ne!(replacement_identity, captured_identity);

    let mut terminal = None;
    execute_prepare(
        start.command,
        &start.cancellation,
        &start.controls,
        |fact| {
            if let ExecutorFact::Terminal {
                token: terminal_token,
                report,
            } = fact
            {
                assert_eq!(terminal_token, token);
                terminal = Some(report);
            }
        },
    );
    assert_eq!(
        terminal.expect("stale queued request terminates").outcome,
        TerminalOutcome::Failed
    );
    assert!(std::fs::read_dir(&destination).unwrap().next().is_none());
    assert!(std::fs::read_dir(&displaced).unwrap().next().is_none());
}
