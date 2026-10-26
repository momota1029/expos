use super::{executor::ExecutorFact, model::AttemptToken};
use std::sync::{Arc, Mutex};

const RELIABLE_CAPACITY: usize = 64;

/// Per-attempt fact route. Reliable facts retain FIFO order; progress keeps
/// only the latest sequence until the application pump accepts it.
pub(crate) struct WorkerFactTransport {
    reliable: async_channel::Receiver<ExecutorFact>,
    progress: Arc<Mutex<Option<(AttemptToken, super::model::ProgressUpdate)>>>,
    wake: async_channel::Receiver<()>,
}

#[derive(Clone)]
pub(crate) struct WorkerFactSender {
    attempt: AttemptToken,
    reliable: async_channel::Sender<ExecutorFact>,
    progress: Arc<Mutex<Option<(AttemptToken, super::model::ProgressUpdate)>>>,
    wake: async_channel::Sender<()>,
}

impl WorkerFactTransport {
    pub(crate) fn new(attempt: AttemptToken) -> (Self, WorkerFactSender) {
        let (reliable_sender, reliable) = async_channel::bounded(RELIABLE_CAPACITY);
        let (wake_sender, wake) = async_channel::bounded(1);
        let progress = Arc::new(Mutex::new(None));
        (
            Self {
                reliable,
                progress: progress.clone(),
                wake,
            },
            WorkerFactSender {
                attempt,
                reliable: reliable_sender,
                progress,
                wake: wake_sender,
            },
        )
    }

    /// Reliable facts are checked before progress every time the pump wakes.
    /// A wake is only a hint; the queue and mailbox remain the source of data.
    pub(crate) async fn recv(&self) -> Option<ExecutorFact> {
        loop {
            match self.reliable.try_recv() {
                Ok(fact) => return Some(fact),
                Err(async_channel::TryRecvError::Closed) => {}
                Err(async_channel::TryRecvError::Empty) => {}
            }
            if let Some((token, update)) = self
                .progress
                .lock()
                .expect("progress mailbox poisoned")
                .take()
            {
                return Some(ExecutorFact::Progress { token, update });
            }
            match self.wake.recv().await {
                Ok(()) => {}
                Err(_) => {
                    // Closure only permits absence after both data routes drain.
                    if let Ok(fact) = self.reliable.try_recv() {
                        return Some(fact);
                    }
                    if let Some((token, update)) = self
                        .progress
                        .lock()
                        .expect("progress mailbox poisoned")
                        .take()
                    {
                        return Some(ExecutorFact::Progress { token, update });
                    }
                    return None;
                }
            }
        }
    }
}

impl WorkerFactSender {
    pub(crate) async fn send(
        &self,
        fact: ExecutorFact,
    ) -> Result<(), async_channel::SendError<ExecutorFact>> {
        match fact {
            ExecutorFact::Progress { token, update } => {
                self.publish_progress(token, update);
                Ok(())
            }
            fact => {
                self.reliable.send(fact).await?;
                self.notify();
                Ok(())
            }
        }
    }

    pub(crate) fn publish(&self, fact: ExecutorFact) {
        match fact {
            ExecutorFact::Progress { token, update } => self.publish_progress(token, update),
            fact => {
                let _ = self.reliable.send_blocking(fact);
                self.notify();
            }
        }
    }

    fn publish_progress(&self, token: AttemptToken, update: super::model::ProgressUpdate) {
        // The mailbox belongs to one claimed attempt. A malformed or stale
        // producer token cannot evict that attempt's latest accepted update.
        if token != self.attempt {
            return;
        }
        let mut pending = self.progress.lock().expect("progress mailbox poisoned");
        let replace = pending
            .as_ref()
            .is_none_or(|(_, current)| update.sequence > current.sequence);
        if replace {
            *pending = Some((token, update));
        }
        drop(pending);
        self.notify();
    }

    fn notify(&self) {
        // A full one-slot wake queue already guarantees a pump wakeup.
        let _ = self.wake.try_send(());
    }

    pub(crate) fn attempt(&self) -> AttemptToken {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll, Wake, Waker},
    };

    #[derive(Default)]
    struct TestWake(AtomicBool);

    impl Wake for TestWake {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn test_context() -> (Arc<TestWake>, Waker) {
        let wake = Arc::new(TestWake::default());
        (wake.clone(), Waker::from(wake))
    }

    fn request() -> super::super::model::OperationRequest {
        super::super::model::OperationRequest {
            kind: super::super::model::OperationKind::Copy,
            virtual_source: None,
            sources: vec!["source".into()],
            destination: Some("target".into()),
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
        }
    }

    fn attempts() -> (AttemptToken, AttemptToken) {
        let mut runtime = super::super::runtime::OperationRuntime::new(
            super::super::model::OperationService::default(),
        );
        let first = runtime.submit(request()).expect("first attempt token");
        let second = runtime.submit(request()).expect("second attempt token");
        (first, second)
    }

    fn attempt() -> AttemptToken {
        attempts().0
    }

    fn progress(token: AttemptToken, sequence: u64) -> ExecutorFact {
        ExecutorFact::Progress {
            token,
            update: super::super::model::ProgressUpdate {
                sequence,
                counters: Default::default(),
                current_item: None,
            },
        }
    }

    #[test]
    fn progress_coalesces_and_reliable_facts_keep_fifo() {
        let (token, stale_token) = attempts();
        let (transport, sender) = WorkerFactTransport::new(token);
        for sequence in 1..=10_000 {
            sender.publish(progress(token, sequence));
        }
        sender.publish(progress(token, 9));
        sender.publish(progress(stale_token, 20_000));
        assert_eq!(
            sender.progress.lock().unwrap().as_ref().unwrap().1.sequence,
            10_000
        );
        assert!(sender.reliable.is_empty());
        assert_eq!(sender.attempt(), token);
        for description in ["first", "second"] {
            sender.publish(ExecutorFact::Conflict {
                token,
                notice: super::super::model::ConflictNotice {
                    item: super::super::model::ItemId(0),
                    probe_revision: 1,
                    description: description.into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            });
        }
        for expected in ["first", "second"] {
            match transport.reliable.try_recv().unwrap() {
                ExecutorFact::Conflict {
                    token: actual_token,
                    notice,
                } => {
                    assert_eq!(actual_token, token);
                    assert_eq!(notice.description, expected);
                }
                _ => panic!("reliable route preserves conflict facts"),
            }
        }
    }

    #[test]
    fn publish_between_mailbox_take_and_wait_leaves_a_wakeup() {
        let token = attempt();
        let (transport, sender) = WorkerFactTransport::new(token);
        sender.publish(progress(token, 1));
        assert!(transport.progress.lock().unwrap().take().is_some());

        // This is the producer interleaving after the pump's empty check and
        // before it parks: the mailbox write and wake publication are paired.
        sender.publish(progress(token, 2));
        assert_eq!(transport.wake.try_recv(), Ok(()));
        assert_eq!(
            transport
                .progress
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .sequence,
            2
        );
    }

    #[test]
    fn recv_registers_waker_and_delivers_progress_and_reliable_facts() {
        let token = attempt();
        let (transport, sender) = WorkerFactTransport::new(token);
        let (wake, waker) = test_context();
        let mut cx = Context::from_waker(&waker);

        let mut receive = Box::pin(transport.recv());
        assert!(matches!(receive.as_mut().poll(&mut cx), Poll::Pending));
        assert!(!wake.0.load(Ordering::SeqCst));
        sender.publish(progress(token, 1));
        assert!(wake.0.load(Ordering::SeqCst));
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            Poll::Ready(Some(ExecutorFact::Progress { token: got, update }))
                if got == token && update.sequence == 1
        ));

        wake.0.store(false, Ordering::SeqCst);
        let mut receive_reliable = Box::pin(transport.recv());
        assert!(matches!(
            receive_reliable.as_mut().poll(&mut cx),
            Poll::Pending
        ));
        sender.publish(ExecutorFact::Conflict {
            token,
            notice: super::super::model::ConflictNotice {
                item: super::super::model::ItemId(0),
                probe_revision: 1,
                description: "wake on reliable fact".into(),
                replace_supported: false,
                directory_move_replace_rename_candidate: None,
            },
        });
        assert!(wake.0.load(Ordering::SeqCst));
        assert!(matches!(
            receive_reliable.as_mut().poll(&mut cx),
            Poll::Ready(Some(ExecutorFact::Conflict { notice, .. }))
                if notice.description == "wake on reliable fact"
        ));
    }

    #[test]
    fn recv_drains_reliable_before_progress_after_closure_and_runtime_rejects_late_progress() {
        let mut runtime = super::super::runtime::OperationRuntime::new(
            super::super::model::OperationService::default(),
        );
        let token = runtime.submit(request()).unwrap();
        runtime.start_next().unwrap();
        let (transport, sender) = WorkerFactTransport::new(token);
        let late_progress = progress(token, 7);
        let terminal = ExecutorFact::Terminal {
            token,
            report: super::super::model::TerminalReport {
                outcome: super::super::model::TerminalOutcome::Unknown,
                final_counters: Default::default(),
                receipts: Vec::new(),
                retry: super::super::model::RetryPlan::NotAllowed,
                change_hints: Vec::new(),
            },
        };
        sender.publish(ExecutorFact::BatchRenameItemCompleted {
            token,
            item: super::super::model::ItemId(0),
            source: "source".into(),
            destination: "target/source".into(),
            source_identity: "source-id".into(),
            parent_identity: "parent-id".into(),
        });
        sender.publish(terminal);
        sender.publish(late_progress.clone());
        drop(sender);

        let (_, waker) = test_context();
        let mut cx = Context::from_waker(&waker);
        let mut receive = Box::pin(transport.recv());
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            Poll::Ready(Some(ExecutorFact::BatchRenameItemCompleted { .. }))
        ));
        drop(receive);
        let mut receive = Box::pin(transport.recv());
        let terminal = match receive.as_mut().poll(&mut cx) {
            Poll::Ready(Some(fact @ ExecutorFact::Terminal { .. })) => fact,
            other => panic!("expected terminal after the reliable receipt, got {other:?}"),
        };
        drop(receive);
        let mut receive = Box::pin(transport.recv());
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            Poll::Ready(Some(ExecutorFact::Progress { update, .. })) if update.sequence == 7
        ));
        drop(receive);
        let mut receive = Box::pin(transport.recv());
        assert!(matches!(receive.as_mut().poll(&mut cx), Poll::Ready(None)));

        assert!(runtime.commit_fact(terminal).is_some());
        let ExecutorFact::Progress { token, update } = late_progress else {
            unreachable!("test fact is progress")
        };
        assert!(!runtime.commit_progress(token, &update));
    }

    #[test]
    fn cancellation_flag_sets_while_reliable_route_is_full() {
        use std::sync::atomic::Ordering;

        let mut runtime = super::super::runtime::OperationRuntime::new(
            super::super::model::OperationService::default(),
        );
        let token = runtime.submit(request()).unwrap();
        let worker = runtime.start_next().unwrap();
        let (transport, sender) = WorkerFactTransport::new(token);
        for _ in 0..RELIABLE_CAPACITY {
            sender.publish(ExecutorFact::Conflict {
                token,
                notice: super::super::model::ConflictNotice {
                    item: super::super::model::ItemId(0),
                    probe_revision: 1,
                    description: "bounded fact".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            });
        }
        assert_eq!(sender.reliable.len(), RELIABLE_CAPACITY);

        let _ = runtime.cancel(token);
        assert!(worker.cancellation.load(Ordering::Acquire));
        assert_eq!(sender.reliable.len(), RELIABLE_CAPACITY);
        drop(transport);
    }

    #[test]
    fn progress_saturation_preserves_cancel_control_and_terminal_report() {
        use super::super::{executor::WorkerControl, model::AttemptState};

        let mut runtime = super::super::runtime::OperationRuntime::new(
            super::super::model::OperationService::default(),
        );
        let token = runtime.submit(request()).unwrap();
        let worker = runtime.start_next().unwrap();
        let (transport, sender) = WorkerFactTransport::new(token);

        for sequence in 1..=10_000 {
            sender.publish(progress(token, sequence));
        }
        assert_eq!(
            sender.progress.lock().unwrap().as_ref().unwrap().1.sequence,
            10_000
        );

        for _ in 0..RELIABLE_CAPACITY {
            sender.publish(ExecutorFact::Conflict {
                token,
                notice: super::super::model::ConflictNotice {
                    item: super::super::model::ItemId(0),
                    probe_revision: 1,
                    description: "fill reliable route".into(),
                    replace_supported: false,
                    directory_move_replace_rename_candidate: None,
                },
            });
        }
        assert_eq!(sender.reliable.len(), RELIABLE_CAPACITY);

        let _ = runtime.cancel(token);
        assert!(
            worker
                .cancellation
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(worker.controls.try_recv(), Ok(WorkerControl::Cancel));
        assert_eq!(
            runtime.job(token).unwrap().attempts[0].state,
            AttemptState::CancelRequested
        );

        let final_report = super::super::model::TerminalReport {
            outcome: super::super::model::TerminalOutcome::Succeeded,
            final_counters: super::super::model::ProgressCounters {
                processed_items: 1,
                processed_bytes: 64,
                ..Default::default()
            },
            receipts: vec![super::super::model::ItemReceipt {
                item: super::super::model::ItemId(0),
                outcome: super::super::model::ItemOutcome::Completed,
                detail: None,
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None, copy_undo: None,
            }],
            retry: super::super::model::RetryPlan::NotAllowed,
            change_hints: vec![super::super::model::FsChangeHint {
                directory: std::path::PathBuf::from("target"),
            }],
        };
        let mut terminal_send = Box::pin(sender.send(ExecutorFact::Terminal {
            token,
            report: final_report.clone(),
        }));
        let (_, waker) = test_context();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            terminal_send.as_mut().poll(&mut cx),
            Poll::Pending
        ));

        let mut terminal_enqueued = false;
        for _ in 0..RELIABLE_CAPACITY {
            let mut receive = Box::pin(transport.recv());
            match receive.as_mut().poll(&mut cx) {
                Poll::Ready(Some(ExecutorFact::Conflict { token: got, .. })) => {
                    assert_eq!(got, token);
                }
                other => panic!("reliable facts must drain before Terminal: {other:?}"),
            }
            if !terminal_enqueued && sender.reliable.len() == RELIABLE_CAPACITY - 1 {
                assert!(matches!(
                    terminal_send.as_mut().poll(&mut cx),
                    Poll::Ready(Ok(()))
                ));
                terminal_enqueued = true;
            }
        }
        assert!(terminal_enqueued);

        let mut receive_terminal = Box::pin(transport.recv());
        let terminal = match receive_terminal.as_mut().poll(&mut cx) {
            Poll::Ready(Some(fact @ ExecutorFact::Terminal { .. })) => fact,
            other => panic!("Terminal must follow the reliable FIFO: {other:?}"),
        };
        assert!(runtime.commit_fact(terminal).is_some());
        let committed = runtime.job(token).unwrap();
        assert_eq!(committed.attempts[0].state, AttemptState::Terminal);
        assert_eq!(committed.attempts[0].terminal.as_ref(), Some(&final_report));

        let mut receive_progress = Box::pin(transport.recv());
        let ExecutorFact::Progress { token, update } =
            (match receive_progress.as_mut().poll(&mut cx) {
                Poll::Ready(Some(fact)) => fact,
                other => panic!("latest progress must remain drainable: {other:?}"),
            })
        else {
            panic!("the one-slot mailbox retains its latest progress fact");
        };
        assert_eq!(update.sequence, 10_000);
        assert!(!runtime.commit_progress(token, &update));
        assert_eq!(
            runtime.job(token).unwrap().attempts[0].terminal.as_ref(),
            Some(&final_report)
        );

        drop(terminal_send);
        drop(sender);
        let mut receive_closed = Box::pin(transport.recv());
        assert!(matches!(
            receive_closed.as_mut().poll(&mut cx),
            Poll::Ready(None)
        ));
    }
}
