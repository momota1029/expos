use super::*;
use gpui::{App, Application, WindowHandle, WindowOptions};
use gpui_filer_resources::ResourceAssets;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_CLOSE};
use windows::core::PCWSTR;

#[test]
#[ignore = "requires an interactive Windows desktop and a fixed NTFS test volume"]
fn e7_replace_move_completes_after_origin_native_window_close() {
    assert!(
        matches!(std::env::var("GPUI_FILER_INTERACTIVE_HWND_TEST").as_deref(), Ok("1")),
        "this test opens native windows and requires an isolated test desktop; set GPUI_FILER_INTERACTIVE_HWND_TEST=1 only in that environment",
    );
    use operations::journal::{JournalStore, JournalWriter};

    let fixture = tempfile::Builder::new()
        .prefix("gpui-filer-live-e7-")
        .tempdir_in(std::env::current_dir().expect("current directory"))
        .expect("create same-volume fixture");
    let source_parent = fixture.path().join("source-parent");
    let destination_parent = fixture.path().join("destination-parent");
    std::fs::create_dir_all(&source_parent).expect("create source parent");
    std::fs::create_dir_all(&destination_parent).expect("create destination parent");
    let source = source_parent.join("incoming");
    let destination = destination_parent.join("incoming");
    std::fs::create_dir_all(source.join("nested")).expect("create source tree");
    std::fs::write(source.join("nested/source.bin"), b"incoming tree identity")
        .expect("write source");
    std::fs::create_dir_all(destination.join("nested")).expect("create existing destination tree");
    std::fs::write(
        destination.join("nested/target.bin"),
        b"displaced target identity",
    )
    .expect("write target");
    let copy_source = fixture.path().join("queued-copy.bin");
    let copy_destination = fixture.path().join("queued-copy-output");
    std::fs::write(&copy_source, b"queued copy remains behind the E7 lane")
        .expect("write queued copy source");
    std::fs::create_dir(&copy_destination).expect("create queued copy destination");
    assert_fixed_ntfs(fixture.path());
    let source_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
        &source,
        crate::directory::EntryKind::Folder,
    )
    .expect("source identity")
    .filesystem_identity();
    let destination_parent_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
        &destination_parent,
        crate::directory::EntryKind::Folder,
    )
    .expect("destination parent identity")
    .filesystem_identity();
    let target_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
        &destination,
        crate::directory::EntryKind::Folder,
    )
    .expect("target identity")
    .filesystem_identity();
    let source_snapshot = snapshot_tree(&source).expect("snapshot source E7 tree");
    let target_snapshot = snapshot_tree(&destination).expect("snapshot displaced E7 tree");
    let journal_path = fixture.path().join("journal-v1.json");
    let store = JournalStore::open(journal_path).expect("open fixture journal");
    let real_writer = Arc::new(JournalWriter::start(
        JournalStore::open(fixture.path().join("journal-v1.json"))
            .expect("open real journal writer"),
    ));
    let (test_writer, journal_driver) = JournalWriter::test_driver();
    let result = Arc::new(Mutex::new(None::<Result<(), String>>));
    let controller_result = result.clone();
    let final_result = result.clone();
    let operation_tokens = Arc::new(Mutex::new(Vec::<operations::model::AttemptToken>::new()));
    let watchdog_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_done = watchdog_finished.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(120));
        if !watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
            eprintln!("FAIL: E7 native HWND test exceeded its 120 second ultimate watchdog");
            std::process::exit(124);
        }
    });
    let app = Application::with_platform(std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("initialize live Windows platform"),
    ))
    .with_assets(ResourceAssets)
    .with_quit_mode(gpui::QuitMode::Explicit);

    app.run(move |cx: &mut App| {
        cx.set_global(persistence::SessionAutosaveOwner::disabled());
        cx.set_global(persistence::FolderPreferencesOwner::disabled());
        cx.set_global(window_coordinator::WindowCoordinator::default());
        let mut owner = operations::application::OperationApplicationOwner::for_test_with_journal_store(store);
        owner.journal = Some(test_writer);
        cx.set_global(owner);
        cx.set_global(crate::show_hidden_items::ShowHiddenItems::default());
        cx.set_global(crate::ItemCheckBoxes::default());
        cx.set_global(crate::archive_provider::ArchiveProviderOwner::default());

        let (phase_ack_tx, phase_ack_rx) = async_channel::bounded(1);
        let (release_phase_tx, release_phase_rx) = async_channel::bounded(1);
        let (release_worker_return_tx, release_worker_return_rx) = async_channel::bounded(1);
        let cleanup_release_phase_tx = release_phase_tx.clone();
        let cleanup_release_worker_return_tx = release_worker_return_tx.clone();
        let operation_tokens = operation_tokens.clone();
        let (complete_attempt_tx, complete_attempt_rx) = async_channel::unbounded();
        let (published_attempt_tx, published_attempt_rx) = async_channel::bounded(1);
        let journal_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let return_gate_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
        cx.global_mut::<operations::application::OperationApplicationOwner>()
            .configure_worker_pump_test({
                let return_gate_once = return_gate_once.clone();
                move |worker, fact_sender| {
                    let command = worker.command;
                    let cancellation = worker.cancellation;
                    let controls = worker.controls;
                    let phase_ack_tx = phase_ack_tx.clone();
                    let release_phase_rx = release_phase_rx.clone();
                    let hold_return = return_gate_once.swap(false, std::sync::atomic::Ordering::AcqRel);
                    let (finished_tx, finished_rx) = async_channel::bounded::<()>(1);
                    let thread = std::thread::spawn(move || {
                        let (executor_control_tx, executor_control_rx) = std::sync::mpsc::channel();
                        let phase_ack_tx = phase_ack_tx.clone();
                        let release_phase_rx = release_phase_rx.clone();
                        let forwarder = std::thread::spawn(move || {
                            while let Ok(control) = controls.recv() {
                                if let executor::WorkerControl::DirectoryMoveReplaceRenamePhaseJournaledExact {
                                    token, conflict_token, ledger,
                                } = &control
                                    && ledger.phase == operations::journal::DirectoryMoveReplaceRenamePhase::DestinationPublished
                                {
                                    let _ = phase_ack_tx.send_blocking((*token, *conflict_token, ledger.clone()));
                                    let _ = release_phase_rx.recv_blocking();
                                }
                                if executor_control_tx.send(control).is_err() {
                                    break;
                                }
                            }
                        });
                        executor::execute_prepare_with_directory_move_replace_rename_enabled_for_test(
                            command,
                            cancellation.as_ref(),
                            &executor_control_rx,
                            None,
                            |fact| fact_sender.publish(fact),
                        );
                        drop(fact_sender);
                        let _ = forwarder.join();
                        let _ = finished_tx.send_blocking(());
                    });
                    Box::pin(async move {
                        let _ = finished_rx.recv().await;
                        let _ = thread.join();
                        if hold_return {
                            let _ = release_worker_return_rx.recv().await;
                        }
                    })
                }
            });

        let stop_journal = journal_stop.clone();
        let journal_writer = real_writer.clone();
        std::thread::spawn(move || {
            loop {
              if stop_journal.load(std::sync::atomic::Ordering::Acquire) { break; }
              if let Some(command) = journal_driver.try_command() {
                let published = match &command {
                    operations::journal::JournalWriterCommandForTest::Upsert(upsert)
                        if upsert.attempt.directory_move_replace_rename.as_ref().is_some_and(|ledger|
                            ledger.phase == operations::journal::DirectoryMoveReplaceRenamePhase::DestinationPublished) => Some(upsert.attempt.clone()),
                    _ => None,
                };
                let completion = match &command {
                    operations::journal::JournalWriterCommandForTest::Upsert(upsert)
                        if upsert.attempt.state == operations::journal::JournalState::Terminal => Some(upsert.attempt.clone()),
                    _ => None,
                };
                let persisted = command.persist_to(&journal_writer);
                match command {
                    operations::journal::JournalWriterCommandForTest::Upsert(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::UnacknowledgedUpsert(_) => {}
                    operations::journal::JournalWriterCommandForTest::CrossVolumeMoveReplace(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveManifest(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveChild(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveRoot(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveComplete(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveReplacePhase(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveReplaceBackupChild(command) => command.reply(persisted.clone()),
                    operations::journal::JournalWriterCommandForTest::DirectoryMoveReplaceBackupComplete(command) => command.reply(persisted.clone()),
                }
                if let Some(attempt) = published {
                    if persisted.is_ok() {
                        let durable = journal_writer.read_attempt(attempt.key.clone()).recv_blocking()
                            .unwrap_or_else(|_| Err("real writer readback channel closed".into()))
                            .and_then(|result| result.ok_or_else(|| "durable published attempt missing".into()));
                        let _ = published_attempt_tx.send_blocking((attempt, durable));
                    }
                }
                if let Some(attempt) = completion {
                    let durable = journal_writer.read_attempt(attempt.key.clone()).recv_blocking()
                        .unwrap_or_else(|_| Err("real writer readback channel closed".into()))
                        .and_then(|result| result.ok_or_else(|| "durable attempt missing".into()));
                    let _ = complete_attempt_tx.send_blocking((attempt, durable));
                }
              } else {
                std::thread::sleep(Duration::from_millis(1));
              }
            }
        });

        let origin_hwnd = Arc::new(Mutex::new(None));
        let survivor_hwnd = Arc::new(Mutex::new(None));
        let keymap = Arc::new(input::KeymapRuntime::new());
        let origin = match open_shell(cx, 1, origin_hwnd.clone(), keymap.clone()) {
            Ok(opened) => opened,
            Err(error) => {
                spawn_setup_cleanup(
                    cx, error, result.clone(), journal_stop.clone(),
                    origin_hwnd.clone(), survivor_hwnd.clone(),
                );
                return;
            }
        };
        let survivor = match open_shell(cx, 2, survivor_hwnd.clone(), keymap) {
            Ok(opened) => opened,
            Err(error) => {
                spawn_setup_cleanup(
                    cx, error, result.clone(), journal_stop.clone(),
                    origin_hwnd.clone(), survivor_hwnd.clone(),
                );
                return;
            }
        };
        let tabs = survivor.0.update(cx, |shell, _, cx| {
            let pane_id = shell.workspace.active().focused_pane_id;
            shell.add_tab_in(pane_id, Some(Location::Path(source_parent.clone())), cx);
            shell.add_tab_in(pane_id, Some(Location::Path(destination_parent.clone())), cx);
            let mut source_view = None;
            let mut destination_view = None;
            for tab in shell.workspace.active().tabs() {
                match tab.view.read(cx).location().as_ref().and_then(Location::path) {
                    Some(path) if path == source_parent.as_path() => source_view = Some(tab.view.clone()),
                    Some(path) if path == destination_parent.as_path() => destination_view = Some(tab.view.clone()),
                    _ => {}
                }
            }
            source_view.zip(destination_view)
        });
        let Some((source_view, destination_view)) = tabs.ok().flatten() else {
            spawn_setup_cleanup(
                cx, "survivor Shell could not create both fixture tabs".into(), result.clone(),
                journal_stop.clone(), origin_hwnd.clone(), survivor_hwnd.clone(),
            );
            return;
        };
        let origin_incarnation = origin.1;
        let survivor_incarnation = survivor.1;
        let origin = origin.0;
        let survivor = survivor.0;
        let identities_match = origin.update(cx, |shell, _, _| (shell.window_id, shell.window_incarnation))
            .is_ok_and(|identity| identity == (1, origin_incarnation))
            && survivor.update(cx, |shell, _, _| (shell.window_id, shell.window_incarnation))
                .is_ok_and(|identity| identity == (2, survivor_incarnation))
            && cx.global::<window_coordinator::WindowCoordinator>().contains(1, origin_incarnation)
            && cx.global::<window_coordinator::WindowCoordinator>().contains(2, survivor_incarnation);
        if !identities_match {
            spawn_setup_cleanup(
                cx,
                "origin/survivor Shell identity does not match WindowCoordinator registrations".into(),
                result.clone(),
                journal_stop.clone(),
                origin_hwnd.clone(),
                survivor_hwnd.clone(),
            );
            return;
        }

        let controller_stop = journal_stop.clone();
        cx.spawn(async move |mut cx| {
            let deadline = Instant::now() + Duration::from_secs(45);
            let run_result = async {
                wait_for_shells_observed(
                    &mut cx,
                    &origin,
                    &survivor,
                    deadline,
                )
                .await?;

                let origin_raw = origin_hwnd
                    .lock()
                    .map_err(|_| "origin HWND lock poisoned")?
                    .ok_or("origin HWND not captured")?;
                let survivor_raw = survivor_hwnd
                    .lock()
                    .map_err(|_| "survivor HWND lock poisoned")?
                    .ok_or("survivor HWND not captured")?;
                run_e7_scenario(
                    &mut cx,
                    &origin,
                    &survivor,
                    &source_view,
                    &destination_view,
                    &source_parent,
                    &source,
                    &destination_parent,
                    &source_identity,
                    &destination_parent_identity,
                    &copy_source,
                    &copy_destination,
                    &source_snapshot,
                    &target_snapshot,
                    &target_identity,
                    phase_ack_rx,
                    published_attempt_rx,
                    release_phase_tx,
                    release_worker_return_tx,
                    complete_attempt_rx,
                    real_writer.clone(),
                    operation_tokens.clone(),
                    1,
                    2,
                    origin_incarnation,
                    survivor_incarnation,
                    origin_raw,
                    survivor_raw,
                    deadline,
                )
                .await?;
                post_close(survivor_raw)?;
                wait_for_window_destroyed(
                    &mut cx,
                    &survivor,
                    2,
                    survivor_incarnation,
                    survivor_raw,
                    deadline,
                )
                .await
            }
            .await;

            let run_result = if let Err(error) = run_result {
                eprintln!("FAIL: E7 native HWND scenario failed: {error}");
                if let Ok(mut output) = controller_result.lock() {
                    *output = Some(Err(error.clone()));
                }
                let _ = cleanup_release_phase_tx.try_send(());
                let _ = cleanup_release_worker_return_tx.try_send(());
                Err(cleanup_failed_scenario(
                    &mut cx,
                    error,
                    &origin,
                    &survivor,
                    1,
                    2,
                    origin_incarnation,
                    survivor_incarnation,
                    &origin_hwnd,
                    &survivor_hwnd,
                    &operation_tokens,
                    &real_writer,
                ).await)
            } else { run_result };
            if let Ok(mut output) = controller_result.lock() {
                *output = Some(run_result);
            }
            controller_stop.store(true, std::sync::atomic::Ordering::Release);
            let _ = cx.update(|app| app.quit());
        })
        .detach();
    });
    watchdog_finished.store(true, std::sync::atomic::Ordering::Release);

    final_result
        .lock()
        .expect("controller result lock")
        .take()
        .expect("controller returned a result")
        .expect("live GPUI native window lifecycle");
}

fn open_shell(
    cx: &mut App,
    id: u64,
    hwnd: Arc<Mutex<Option<usize>>>,
    keymap: Arc<input::KeymapRuntime>,
) -> Result<(WindowHandle<FilerShell>, u64), String> {
    let startup = persistence::Startup {
        window_id: id,
        ..persistence::Startup::disabled()
    };
    let handle = cx
        .open_window(WindowOptions::default(), move |window, cx| {
            if let Some(native) = native_window_handle(window) {
                if let Ok(mut captured) = hwnd.lock() {
                    *captured = Some(native.0 as usize);
                }
            }
            cx.new(|cx| FilerShell::new(startup, keymap, Vec::new(), cx))
        })
        .map_err(|error| format!("open live GPUI window {id}: {error}"))?;
    let incarnation = cx
        .global_mut::<window_coordinator::WindowCoordinator>()
        .register(id, handle)
        .ok_or_else(|| format!("register live GPUI window {id}: duplicate registration"))?;
    let shell_identity = handle
        .update(cx, |shell, _, _| {
            shell.set_window_incarnation(incarnation);
            (shell.window_id, shell.window_incarnation)
        })
        .map_err(|error| format!("set window incarnation for {id}: {error}"))?;
    if shell_identity != (id, incarnation)
        || !cx
            .global::<window_coordinator::WindowCoordinator>()
            .contains(id, incarnation)
    {
        return Err(format!(
            "Shell/coordinator identity mismatch for window {id}"
        ));
    }
    Ok((handle, incarnation))
}

async fn cleanup_failed_scenario(
    cx: &mut gpui::AsyncApp,
    error: String,
    origin: &WindowHandle<FilerShell>,
    survivor: &WindowHandle<FilerShell>,
    origin_id: u64,
    survivor_id: u64,
    origin_incarnation: u64,
    survivor_incarnation: u64,
    origin_hwnd: &Arc<Mutex<Option<usize>>>,
    survivor_hwnd: &Arc<Mutex<Option<usize>>>,
    operation_tokens: &Arc<Mutex<Vec<operations::model::AttemptToken>>>,
    real_writer: &Arc<operations::journal::JournalWriter>,
) -> String {
    use operations::journal::JournalState;

    let (tokens, token_list_readable) = match operation_tokens.lock() {
        Ok(tokens) => (tokens.clone(), true),
        Err(_) => {
            eprintln!("FAIL: E7 cleanup could not read accepted operation tokens");
            (Vec::new(), false)
        }
    };
    let (keys, commits) = cx.update(|app| {
        let owner = app.global_mut::<operations::application::OperationApplicationOwner>();
        let mut keys = Vec::with_capacity(tokens.len());
        let mut commits = Vec::new();
        for token in &tokens {
            keys.push(
                owner
                    .runtime
                    .journal_attempt(*token)
                    .map(|attempt| attempt.key.clone()),
            );
            if let Some(commit) = owner.runtime.cancel(*token) {
                commits.push(commit);
            }
        }
        (keys, commits)
    });
    for commit in commits {
        let event = commit.event;
        if let Err(error) = survivor.update(cx, |shell, _, cx| {
            shell.record_operation_terminal(&event, cx);
            shell.reconcile_operation_terminal(event, cx);
            shell.record_active_operation(cx);
            shell.start_next_operation(cx);
        }) {
            eprintln!("FAIL: route cancellation terminal through survivor Shell: {error}");
        }
    }
    let mut readers: Vec<
        Option<
            async_channel::Receiver<Result<Option<operations::journal::JournalAttempt>, String>>,
        >,
    > = (0..keys.len()).map(|_| None).collect();
    let mut durable_terminal = vec![false; keys.len()];
    let mut token_status = keys
        .iter()
        .map(|key| {
            key.as_ref().map_or_else(
                || "missing journal key".to_owned(),
                |key| format!("key={key}; readback pending"),
            )
        })
        .collect::<Vec<_>>();
    if let Some(hwnd) = origin_hwnd.lock().ok().and_then(|hwnd| *hwnd) {
        let _ = post_close(hwnd);
    }
    if let Some(hwnd) = survivor_hwnd.lock().ok().and_then(|hwnd| *hwnd) {
        let _ = post_close(hwnd);
    }

    let mut deadline = Instant::now() + Duration::from_secs(10);
    let mut timeout_reported = false;
    loop {
        for index in 0..keys.len() {
            if durable_terminal[index] {
                continue;
            }
            if readers[index].is_none() {
                readers[index] = keys[index]
                    .as_ref()
                    .map(|key| real_writer.read_attempt(key.clone()));
            }
            let Some(reader) = readers[index].as_ref() else {
                continue;
            };
            match reader.try_recv() {
                Ok(Ok(Some(attempt))) => {
                    let key_matches = keys[index].as_deref() == Some(attempt.key.as_str());
                    let outcome_present = attempt.terminal_outcome.is_some();
                    let state_acceptable = attempt.state == JournalState::Terminal
                        || (attempt.state == JournalState::RecoveryRequired && outcome_present);
                    durable_terminal[index] = key_matches && state_acceptable && outcome_present;
                    token_status[index] = format!(
                        "expected_key={}; actual_key={}; key_matches={key_matches}; state={:?}; outcome={:?}; durable_terminal={}",
                        keys[index].as_deref().unwrap_or("<missing>"),
                        attempt.key,
                        attempt.state,
                        attempt.terminal_outcome,
                        durable_terminal[index],
                    );
                    readers[index] = None;
                }
                Ok(Ok(None)) | Err(async_channel::TryRecvError::Closed) => {
                    token_status[index] = format!(
                        "key={}; record missing or read channel closed",
                        keys[index].as_deref().unwrap_or("<missing>")
                    );
                    readers[index] = None;
                }
                Ok(Err(error)) => {
                    eprintln!("FAIL: E7 cleanup journal readback failed: {error}");
                    token_status[index] = format!(
                        "key={}; readback error={error}",
                        keys[index].as_deref().unwrap_or("<missing>")
                    );
                    readers[index] = None;
                }
                Err(async_channel::TryRecvError::Empty) => {}
            }
        }

        let lane_idle = !cx.update(|app| {
            app.global::<operations::application::OperationApplicationOwner>()
                .worker_claimed_for_test()
        });
        let origin_raw = origin_hwnd.lock().ok().and_then(|hwnd| *hwnd);
        let survivor_raw = survivor_hwnd.lock().ok().and_then(|hwnd| *hwnd);
        let (origin_handle_gone, survivor_handle_gone, origin_registered, survivor_registered) = cx
            .update(|app| {
                let origin_handle_gone = origin.update(app, |_, _, _| ()).is_err();
                let survivor_handle_gone = survivor.update(app, |_, _, _| ()).is_err();
                let coordinator = app.global::<window_coordinator::WindowCoordinator>();
                (
                    origin_handle_gone,
                    survivor_handle_gone,
                    coordinator.contains(origin_id, origin_incarnation),
                    coordinator.contains(survivor_id, survivor_incarnation),
                )
            });
        let handles_gone = origin_handle_gone
            && survivor_handle_gone
            && !origin_registered
            && !survivor_registered;
        let origin_hwnd_alive =
            origin_raw.is_some_and(|hwnd| unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() });
        let survivor_hwnd_alive =
            survivor_raw.is_some_and(|hwnd| unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() });
        let hwnds_gone = !origin_hwnd_alive && !survivor_hwnd_alive;
        if token_list_readable
            && lane_idle
            && durable_terminal.iter().all(|terminal| *terminal)
            && handles_gone
            && hwnds_gone
        {
            return if timeout_reported {
                format!(
                    "{error}; error cleanup exceeded 10 seconds before draining durable terminal and native window teardown"
                )
            } else {
                error
            };
        }
        if Instant::now() >= deadline && !timeout_reported {
            eprintln!(
                "FAIL: E7 cleanup deadline: lane_idle={lane_idle}; attempts=[{}]; origin_handle_gone={origin_handle_gone}; survivor_handle_gone={survivor_handle_gone}; origin_registered={origin_registered}; survivor_registered={survivor_registered}; origin_hwnd_alive={origin_hwnd_alive}; survivor_hwnd_alive={survivor_hwnd_alive}; relay remains active",
                token_status.join(" | ")
            );
            timeout_reported = true;
            deadline = Instant::now() + Duration::from_secs(10);
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

fn spawn_setup_cleanup(
    cx: &mut App,
    error: String,
    result: Arc<Mutex<Option<Result<(), String>>>>,
    journal_stop: Arc<std::sync::atomic::AtomicBool>,
    origin_hwnd: Arc<Mutex<Option<usize>>>,
    survivor_hwnd: Arc<Mutex<Option<usize>>>,
) {
    eprintln!("FAIL: E7 setup failed: {error}");
    if let Ok(mut output) = result.lock() {
        *output = Some(Err(error.clone()));
    }
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut timeout_reported = false;
        loop {
            let origin = origin_hwnd.lock().ok().and_then(|hwnd| *hwnd);
            let survivor = survivor_hwnd.lock().ok().and_then(|hwnd| *hwnd);
            if let Some(hwnd) = origin {
                let _ = post_close(hwnd);
            }
            if let Some(hwnd) = survivor {
                let _ = post_close(hwnd);
            }
            let (origin_registered, survivor_registered) = cx.update(|app| {
                let registrations = app.global::<window_coordinator::WindowCoordinator>().registrations();
                (
                    registrations.iter().any(|registration| registration.id == 1),
                    registrations.iter().any(|registration| registration.id == 2),
                )
            });
            let registrations_remain = origin_registered || survivor_registered;
            let origin_alive =
                origin.is_some_and(|hwnd| unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() });
            let survivor_alive =
                survivor.is_some_and(|hwnd| unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() });
            if !origin_alive && !survivor_alive && !registrations_remain {
                if let Ok(mut output) = result.lock() {
                    *output = Some(Err(error));
                }
                journal_stop.store(true, std::sync::atomic::Ordering::Release);
                let _ = cx.update(|app| app.quit());
                return;
            }
            if Instant::now() >= deadline && !timeout_reported {
                eprintln!(
                    "FAIL: E7 setup cleanup exceeded 10 seconds: origin_hwnd_alive={origin_alive}; survivor_hwnd_alive={survivor_alive}; origin_registration_present={origin_registered}; survivor_registration_present={survivor_registered}; continuing until native destruction"
                );
                timeout_reported = true;
            }
            if timeout_reported {
                // Keep the native loop alive until GPUI has processed WM_CLOSE; quitting with an HWND
                // still registered would invalidate the lifecycle assertion this test is guarding.
                let _ = cx
                    .background_executor()
                    .timer(Duration::from_millis(25))
                    .await;
            } else {
                cx.background_executor()
                    .timer(Duration::from_millis(10))
                    .await;
            }
        }
    })
    .detach();
}

async fn run_e7_scenario(
    cx: &mut gpui::AsyncApp,
    origin: &WindowHandle<FilerShell>,
    survivor: &WindowHandle<FilerShell>,
    source_view: &gpui::Entity<crate::DirectoryListDemo>,
    destination_view: &gpui::Entity<crate::DirectoryListDemo>,
    source_parent: &std::path::Path,
    source: &std::path::Path,
    destination_parent: &std::path::Path,
    source_identity: &crate::filesystem_identity::FilesystemIdentity,
    destination_parent_identity: &crate::filesystem_identity::FilesystemIdentity,
    copy_source: &std::path::Path,
    copy_destination: &std::path::Path,
    source_snapshot: &[(
        std::path::PathBuf,
        crate::directory::EntryKind,
        crate::filesystem_identity::FilesystemIdentity,
        Option<Vec<u8>>,
    )],
    target_snapshot: &[(
        std::path::PathBuf,
        crate::directory::EntryKind,
        crate::filesystem_identity::FilesystemIdentity,
        Option<Vec<u8>>,
    )],
    target_identity: &crate::filesystem_identity::FilesystemIdentity,
    phase_ack_rx: async_channel::Receiver<(
        operations::model::AttemptToken,
        operations::model::ConflictToken,
        operations::journal::JournalDirectoryMoveReplaceRename,
    )>,
    published_attempt_rx: async_channel::Receiver<(
        operations::journal::JournalAttempt,
        Result<operations::journal::JournalAttempt, String>,
    )>,
    release_phase_tx: async_channel::Sender<()>,
    release_worker_return_tx: async_channel::Sender<()>,
    complete_attempt_rx: async_channel::Receiver<(
        operations::journal::JournalAttempt,
        Result<operations::journal::JournalAttempt, String>,
    )>,
    real_writer: Arc<operations::journal::JournalWriter>,
    operation_tokens: Arc<Mutex<Vec<operations::model::AttemptToken>>>,
    origin_id: u64,
    survivor_id: u64,
    origin_incarnation: u64,
    survivor_incarnation: u64,
    origin_hwnd: usize,
    survivor_hwnd: usize,
    deadline: Instant,
) -> Result<(), String> {
    use operations::journal::DirectoryMoveReplaceRenamePhase as E7Phase;
    use operations::model::{ConflictDecision, OperationKind, OperationRequest};

    let (source_scan_before, destination_scan_before) = loop {
        let baseline = cx.update(|app| {
            let scan = |view: &gpui::Entity<crate::DirectoryListDemo>,
                        expected: &std::path::Path| {
                let view = view.read(app);
                (view.session.status() == &crate::directory_session::ListingStatus::Ready)
                    .then(|| view.session.accepted_scan_ticket())
                    .flatten()
                    .filter(|ticket| ticket.path == expected)
                    .map(|ticket| ticket.scan_id)
            };
            (
                scan(source_view, source_parent),
                scan(destination_view, destination_parent),
            )
        });
        if let (Some(source_scan), Some(destination_scan)) = baseline {
            break (source_scan, destination_scan);
        }
        if Instant::now() >= deadline {
            return Err(
                "survivor source/destination parent listings did not reach baseline Ready".into(),
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    };
    let source = source.to_path_buf();
    let destination_parent = destination_parent.to_path_buf();
    let request_source_identity = source_identity.clone();
    let request_parent_identity = destination_parent_identity.clone();
    let token = cx.update(|app| {
        origin
            .update(app, |shell, _, cx| {
                let token = cx
                    .global_mut::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .submit(OperationRequest {
                        kind: OperationKind::Move,
                        virtual_source: None,
                        sources: vec![source.clone()],
                        destination: Some(destination_parent.clone()),
                        batch_rename_plan: None,
                        move_identity_guard: Some(operations::model::MoveIdentityGuard {
                            source_identities: vec![request_source_identity.clone()],
                            destination_identity: request_parent_identity.clone(),
                        }),
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
                        origin: None,
                    })
                    .map_err(|error| format!("submit E7 directory Move: {error:?}"))?;
                shell.start_next_operation(cx);
                Ok::<_, String>(token)
            })
            .map_err(|error| format!("origin Shell disappeared during E7 submit: {error}"))?
    })?;
    operation_tokens
        .lock()
        .map_err(|_| "E7 cleanup token lock poisoned")?
        .push(token);
    let e7_key = cx
        .update(|app| {
            app.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .journal_attempt(token)
                .map(|attempt| attempt.key.clone())
        })
        .ok_or("E7 runtime attempt disappeared after admission")?;

    let conflict = loop {
        let found = cx.update(|app| {
            app.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .job(token)
                .and_then(|job| {
                    job.attempts
                        .first()?
                        .pending_conflict
                        .as_ref()
                        .map(|pending| pending.token)
                })
        });
        if let Some(conflict) = found {
            break conflict;
        }
        if Instant::now() >= deadline {
            return Err("E7 Move did not reach its real destination conflict".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    };
    cx.update(|app| {
        origin.update(app, |shell, _, cx| {
            shell.resolve_operation_conflict(conflict, ConflictDecision::Replace, false, cx);
        })
    })
    .map_err(|error| format!("origin Shell disappeared while resolving Replace: {error}"))?;

    let (ack_token, ack_conflict, held_ledger) = loop {
        match phase_ack_rx.try_recv() {
            Ok(ack) => break ack,
            Err(async_channel::TryRecvError::Closed) => {
                return Err("DestinationPublished ACK gate closed".into());
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("E7 did not reach DestinationPublished".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    };
    if ack_token != token
        || ack_conflict != conflict
        || held_ledger.phase != E7Phase::DestinationPublished
    {
        return Err("held ACK did not match the exact E7 attempt/conflict/phase".into());
    }
    let (published_attempt, durable_published) = loop {
        match published_attempt_rx.try_recv() {
            Ok(value) => break value,
            Err(async_channel::TryRecvError::Closed) => {
                return Err("durable DestinationPublished observation closed".into());
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("DestinationPublished was not durable before its ACK".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    };
    let durable_published = durable_published?;
    if published_attempt.key != e7_key
        || durable_published.directory_move_replace_rename.as_ref() != Some(&held_ledger)
        || held_ledger.phase != E7Phase::DestinationPublished
    {
        return Err("durable E7 ledger does not equal the held DestinationPublished ACK".into());
    }
    let backup = journal_path(&held_ledger.backup)?;
    let published_destination = journal_path(&held_ledger.destination)?;
    let (approved_volume, approved_index) = target_identity
        .windows_parts()
        .ok_or("target identity is not a Windows filesystem identity")?;
    if held_ledger.approved_target_identity != format!("{approved_volume}:{approved_index:016x}")
        || held_ledger.source_location
            != operations::journal::DirectoryMoveReplaceSourceLocation::Published
        || held_ledger.target_location
            != operations::journal::DirectoryMoveReplaceTargetLocation::Backup
    {
        return Err(
            "held DestinationPublished ACK contains the wrong displaced target identity/location"
                .into(),
        );
    }
    if source.exists()
        || !crate::directory::EntrySourceIdentity::from_path_no_follow(
            &published_destination,
            crate::directory::EntryKind::Folder,
        )
        .is_some_and(|identity| identity.filesystem_identity() == *source_identity)
        || snapshot_tree(&published_destination)? != source_snapshot
        || snapshot_tree(&backup)? != target_snapshot
        || !backup.exists()
    {
        return Err("held DestinationPublished ACK does not preserve the exact source and displaced destination trees".into());
    }

    let copy_source = copy_source.to_path_buf();
    let copy_destination = copy_destination.to_path_buf();
    let copy_token = cx.update(|app| {
        survivor
            .update(app, |shell, _, cx| {
                let token = cx
                    .global_mut::<operations::application::OperationApplicationOwner>()
                    .runtime
                    .submit(OperationRequest {
                        kind: OperationKind::Copy,
                        virtual_source: None,
                        sources: vec![copy_source.clone()],
                        destination: Some(copy_destination.clone()),
                        batch_rename_plan: None,
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
                        origin: None,
                    })
                    .map_err(|error| format!("queue Copy behind E7: {error:?}"))?;
                shell.start_next_operation(cx);
                Ok::<_, String>(token)
            })
            .map_err(|error| format!("survivor Shell disappeared while queueing Copy: {error}"))?
    })?;
    operation_tokens
        .lock()
        .map_err(|_| "queued Copy cleanup token lock poisoned")?
        .push(copy_token);
    let copy_key = cx
        .update(|app| {
            app.global::<operations::application::OperationApplicationOwner>()
                .runtime
                .journal_attempt(copy_token)
                .map(|attempt| attempt.key.clone())
        })
        .ok_or("queued Copy runtime attempt disappeared after admission")?;
    if copy_destination
        .join(copy_source.file_name().ok_or("Copy source has no name")?)
        .exists()
    {
        return Err("queued Copy mutated its destination before the E7 lane released".into());
    }
    if std::fs::read(&copy_source).ok().as_deref()
        != Some(&b"queued copy remains behind the E7 lane"[..])
    {
        return Err("queued Copy source changed while E7 held the serial lane".into());
    }

    post_close(origin_hwnd)?;
    wait_for_origin_destroyed(
        cx,
        origin,
        survivor,
        origin_id,
        survivor_id,
        origin_incarnation,
        survivor_incarnation,
        origin_hwnd,
        survivor_hwnd,
        deadline,
    )
    .await?;
    let readback = real_writer.read_attempt(e7_key.clone());
    let durable_after_close = loop {
        match readback.try_recv() {
            Ok(Ok(Some(attempt))) => break attempt,
            Ok(Ok(None)) => return Err("E7 ACK ledger disappeared after origin close".into()),
            Ok(Err(error)) => return Err(format!("read E7 ledger after origin close: {error}")),
            Err(async_channel::TryRecvError::Closed) => {
                return Err("E7 readback channel closed after origin close".into());
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("E7 ACK ledger readback timed out after origin close".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    };
    if durable_after_close.state == operations::journal::JournalState::Terminal
        || durable_after_close.directory_move_replace_rename.as_ref() != Some(&held_ledger)
    {
        return Err("durable DestinationPublished ACK ledger changed when origin closed".into());
    }
    let survivor_identity = cx.update(|app| survivor.update(app, |_, _, _| ()).is_ok());
    if !survivor_identity {
        return Err("survivor Shell was removed when origin closed".into());
    }

    release_phase_tx
        .try_send(())
        .map_err(|_| "release E7 DestinationPublished ACK")?;
    let (terminal, durable_terminal) = loop {
        match complete_attempt_rx.try_recv() {
            Ok((attempt, durable)) if attempt.key == e7_key => break (attempt, durable),
            Ok(_) => {}
            Err(async_channel::TryRecvError::Closed) => {
                return Err("journal terminal observer closed".into());
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("E7 terminal did not become durable".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    };
    let durable_terminal = durable_terminal?;
    if terminal.state != operations::journal::JournalState::Terminal
        || terminal.terminal_outcome != Some(operations::journal::JournalTerminalOutcome::Succeeded)
        || durable_terminal != terminal
        || terminal
            .directory_move_replace_rename
            .as_ref()
            .is_none_or(|ledger| ledger.phase != E7Phase::Complete)
    {
        return Err("E7 terminal is not durable Complete/Succeeded".into());
    }
    if source.exists() {
        return Err("E7 source tree remains after successful Move".into());
    }
    if !crate::directory::EntrySourceIdentity::from_path_no_follow(
        &published_destination,
        crate::directory::EntryKind::Folder,
    )
    .is_some_and(|identity| identity.filesystem_identity() == *source_identity)
        || snapshot_tree(&published_destination)? != source_snapshot
        || backup.exists()
    {
        return Err(
            "E7 Complete has incorrect destination identity/tree or retained backup".into(),
        );
    }
    let lane_still_claimed = cx.update(|app| {
        app.global::<operations::application::OperationApplicationOwner>()
            .worker_claimed_for_test()
    });
    if !lane_still_claimed {
        return Err(
            "E7 serial lane released before the separately held native worker return".into(),
        );
    }
    let copy_output =
        copy_destination.join(copy_source.file_name().ok_or("Copy source has no name")?);
    if copy_output.exists()
        || std::fs::read(&copy_source).ok().as_deref()
            != Some(&b"queued copy remains behind the E7 lane"[..])
    {
        return Err("queued Copy mutated before worker return".into());
    }

    release_worker_return_tx
        .try_send(())
        .map_err(|_| "release E7 worker-return gate")?;
    loop {
        match complete_attempt_rx.try_recv() {
            Ok((attempt, durable)) if attempt.key == copy_key => {
                let durable = durable?;
                if attempt.state != operations::journal::JournalState::Terminal
                    || attempt.terminal_outcome
                        != Some(operations::journal::JournalTerminalOutcome::Succeeded)
                    || durable != attempt
                {
                    return Err("queued Copy did not reach durable success".into());
                }
                break;
            }
            Ok(_) => {}
            Err(async_channel::TryRecvError::Closed) => {
                return Err("queued Copy journal observer closed".into());
            }
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("queued Copy did not complete after E7 lane release".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(5))
            .await;
    }
    if std::fs::read(&copy_output).ok().as_deref()
        != Some(&b"queued copy remains behind the E7 lane"[..])
    {
        return Err("queued Copy output bytes do not match its source".into());
    }

    let reconcile_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let listings = cx.update(|app| {
            let source = {
                let view = source_view.read(app);
                view.session.status() == &crate::directory_session::ListingStatus::Ready
                    && view.session.accepted_scan_ticket().is_some_and(|ticket| {
                        ticket.path == source_parent && ticket.scan_id > source_scan_before
                    })
                    && !view
                        .entries
                        .iter()
                        .any(|entry| entry.path.as_deref() == Some(source.as_path()))
            };
            let destination = {
                let view = destination_view.read(app);
                view.session.status() == &crate::directory_session::ListingStatus::Ready
                    && view.session.accepted_scan_ticket().is_some_and(|ticket| {
                        ticket.path == destination_parent
                            && ticket.scan_id > destination_scan_before
                    })
                    && view.entries.iter().any(|entry| {
                        entry.path.as_deref() == Some(published_destination.as_path())
                            && entry.source_identity.as_ref().is_some_and(|identity| {
                                identity.filesystem_identity() == *source_identity
                            })
                    })
            };
            source && destination
        });
        if listings {
            break;
        }
        if Instant::now() >= reconcile_deadline {
            return Err(
                "survivor source/destination listings did not reconcile the E7 Move".into(),
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    Ok(())
}

fn journal_path(path: &operations::journal::JournalPath) -> Result<std::path::PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(
        &path.units,
    )))
}

fn assert_fixed_ntfs(path: &std::path::Path) {
    let mut cursor = path;
    while !cursor.exists() {
        cursor = cursor.parent().expect("fixture has an existing parent");
    }
    let root = cursor.to_string_lossy().chars().take(3).collect::<String>();
    let wide = format!("{root}\0").encode_utf16().collect::<Vec<_>>();
    let mut filesystem = [0u16; 32];
    let ok = unsafe {
        GetDriveTypeW(PCWSTR(wide.as_ptr())) == 3
            && GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
            .is_ok()
    } && String::from_utf16_lossy(&filesystem)
        .trim_end_matches(char::from(0))
        .eq_ignore_ascii_case("NTFS");
    assert!(ok, "live E7 fixture must be on a local fixed NTFS volume");
}

fn snapshot_tree(
    root: &std::path::Path,
) -> Result<
    Vec<(
        std::path::PathBuf,
        crate::directory::EntryKind,
        crate::filesystem_identity::FilesystemIdentity,
        Option<Vec<u8>>,
    )>,
    String,
> {
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        rows: &mut Vec<(
            std::path::PathBuf,
            crate::directory::EntryKind,
            crate::filesystem_identity::FilesystemIdentity,
            Option<Vec<u8>>,
        )>,
    ) -> Result<(), String> {
        let kind = if path.is_dir() {
            crate::directory::EntryKind::Folder
        } else {
            crate::directory::EntryKind::File
        };
        let identity = crate::directory::EntrySourceIdentity::from_path_no_follow(path, kind)
            .ok_or_else(|| format!("read identity {}", path.display()))?
            .filesystem_identity();
        let bytes = (kind == crate::directory::EntryKind::File)
            .then(|| {
                std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
            })
            .transpose()?;
        rows.push((
            path.strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_path_buf(),
            kind,
            identity,
            bytes,
        ));
        if kind == crate::directory::EntryKind::Folder {
            let mut children = std::fs::read_dir(path)
                .map_err(|error| format!("enumerate {}: {error}", path.display()))?
                .map(|entry| {
                    entry
                        .map(|entry| entry.path())
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            children.sort();
            for child in children {
                visit(root, &child, rows)?;
            }
        }
        Ok(())
    }
    let mut rows = Vec::new();
    visit(root, root, &mut rows)?;
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(rows)
}

async fn wait_for_shells_observed(
    cx: &mut gpui::AsyncApp,
    origin: &WindowHandle<FilerShell>,
    survivor: &WindowHandle<FilerShell>,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let observed = cx.update(|cx| {
            let origin = origin
                .update(cx, |shell, _, _| shell.window_observed)
                .unwrap_or(false);
            let survivor = survivor
                .update(cx, |shell, _, _| shell.window_observed)
                .unwrap_or(false);
            origin && survivor
        });
        if observed {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("both live Shell windows were not observed before watchdog".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

async fn wait_for_origin_destroyed(
    cx: &mut gpui::AsyncApp,
    origin: &WindowHandle<FilerShell>,
    survivor: &WindowHandle<FilerShell>,
    origin_id: u64,
    survivor_id: u64,
    origin_incarnation: u64,
    survivor_incarnation: u64,
    origin_hwnd: usize,
    survivor_hwnd: usize,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let state = cx.update(|cx| {
            let origin_alive = origin.update(cx, |_, _, _| ()).is_ok();
            let survivor_alive = survivor.update(cx, |_, _, _| ()).is_ok();
            let coordinator = cx.global::<window_coordinator::WindowCoordinator>();
            (
                origin_alive,
                survivor_alive,
                coordinator.contains(origin_id, origin_incarnation),
                coordinator.contains(survivor_id, survivor_incarnation),
            )
        });
        let origin_hwnd_alive = unsafe { IsWindow(HWND(origin_hwnd as *mut _)).as_bool() };
        let survivor_hwnd_alive = unsafe { IsWindow(HWND(survivor_hwnd as *mut _)).as_bool() };
        if !state.0 && !state.2 && !origin_hwnd_alive {
            if !state.1 || !state.3 || !survivor_hwnd_alive {
                return Err("survivor window was lost while origin closed".into());
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("origin Shell/HWND did not close before watchdog".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

async fn wait_for_window_destroyed(
    cx: &mut gpui::AsyncApp,
    handle: &WindowHandle<FilerShell>,
    id: u64,
    incarnation: u64,
    hwnd: usize,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let state = cx.update(|cx| {
            (
                handle.update(cx, |_, _, _| ()).is_ok(),
                cx.global::<window_coordinator::WindowCoordinator>()
                    .contains(id, incarnation),
            )
        });
        let hwnd_alive = unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() };
        if !state.0 && !state.1 && !hwnd_alive {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("survivor Shell/HWND did not close before watchdog".into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
}

fn post_close(hwnd: usize) -> Result<(), String> {
    unsafe { PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .map_err(|error| format!("PostMessageW(WM_CLOSE) failed: {error}"))
}
