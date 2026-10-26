use super::*;

pub(super) fn execute_virtual_copy(
    token: AttemptToken,
    request: super::super::model::OperationRequest,
    sources: Vec<super::super::virtual_transfer::VirtualFileSource>,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let Some(parent) = request.destination.as_deref() else {
        reject_virtual_batch(token, &sources, emit);
        return;
    };
    if request.kind != OperationKind::Copy || !request.sources.is_empty() {
        reject_virtual_batch(token, &sources, emit);
        return;
    }
    #[cfg(windows)]
    let _destination_route = match windows::lock_virtual_destination(
        parent,
        request.copy_destination_identity_guard.as_ref(),
    ) {
        Ok(route) => route,
        Err(_) => {
            reject_virtual_batch(token, &sources, emit);
            return;
        }
    };
    #[cfg(not(windows))]
    {
        let Ok(metadata) = parent.symlink_metadata() else {
            reject_virtual_batch(token, &sources, emit);
            return;
        };
        if !metadata.is_dir() || is_link_or_reparse_point(parent) {
            reject_virtual_batch(token, &sources, emit);
            return;
        }
        if let Some(expected) = request.copy_destination_identity_guard.as_ref() {
            let current = crate::directory::EntrySourceIdentity::from_path(
                parent,
                crate::directory::EntryKind::Folder,
            )
            .map(|identity| identity.filesystem_identity());
            if current.as_ref() != Some(expected) {
                reject_virtual_batch(token, &sources, emit);
                return;
            }
        }
    }
    let total_bytes = sources
        .iter()
        .try_fold(0_u64, |total, source| total.checked_add(source.size?));
    let discovered_bytes = sources
        .iter()
        .filter_map(|source| source.size)
        .fold(0_u64, u64::saturating_add);
    let mut receipts = Vec::with_capacity(sources.len());
    let mut counters = super::super::model::ProgressCounters {
        discovered_items: sources.len() as u64,
        total_items: Some(sources.len() as u64),
        discovered_bytes,
        total_bytes,
        ..Default::default()
    };
    let mut change_hints = Vec::new();
    let mut progress_sequence = 0_u64;
    let mut processed_bytes = 0_u64;
    let mut remaining_choice = None;
    let mut cancelled_active = false;
    let mut retry = super::super::model::RetryPlan::NotAllowed;
    for (index, source) in sources.iter().cloned().enumerate() {
        if cancelled.load(Ordering::Acquire) {
            receipts.extend(sources[index..].iter().enumerate().map(|(offset, _)| {
                super::super::model::ItemReceipt {
                    item: ItemId((index + offset) as u32),
                    outcome: super::super::model::ItemOutcome::CancelledBeforeStart,
                    detail: None,
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }
            }));
            break;
        }
        let mut item_report = None;
        let mut observed_bytes = processed_bytes;
        execute_virtual_copy_item(
            token,
            parent,
            ItemId(index as u32),
            source,
            cancelled,
            controls,
            &mut remaining_choice,
            &mut |fact| match fact {
                ExecutorFact::Progress { token, mut update } => {
                    progress_sequence = progress_sequence.saturating_add(1);
                    update.sequence = progress_sequence;
                    update.counters.processed_items = index as u64;
                    update.counters.discovered_items = sources.len() as u64;
                    update.counters.total_items = Some(sources.len() as u64);
                    update.current_item = Some(ItemId(index as u32));
                    update.counters.processed_bytes =
                        processed_bytes.saturating_add(update.counters.processed_bytes);
                    observed_bytes = observed_bytes.max(update.counters.processed_bytes);
                    update.counters.total_bytes = total_bytes;
                    update.counters.discovered_bytes = discovered_bytes;
                    emit(ExecutorFact::Progress { token, update });
                }
                ExecutorFact::Terminal { report, .. } => item_report = Some(report),
                other => emit(other),
            },
        );
        let Some(mut report) = item_report else {
            receipts.push(super::super::model::ItemReceipt {
                item: ItemId(index as u32),
                outcome: super::super::model::ItemOutcome::Failed,
                detail: Some("virtual copy worker ended without an item report".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            counters.processed_items = (index + 1) as u64;
            continue;
        };
        cancelled_active |= report.outcome == super::super::model::TerminalOutcome::Cancelled;
        let Some(mut receipt) = report.receipts.pop() else {
            receipts.push(super::super::model::ItemReceipt {
                item: ItemId(index as u32),
                outcome: super::super::model::ItemOutcome::Failed,
                detail: Some("virtual copy worker returned no item receipt".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            });
            counters.processed_items = (index + 1) as u64;
            continue;
        };
        receipt.item = ItemId(index as u32);
        processed_bytes = observed_bytes
            .max(processed_bytes.saturating_add(report.final_counters.processed_bytes));
        counters.processed_bytes = processed_bytes;
        counters.processed_items = (index + 1) as u64;
        change_hints.extend(report.change_hints);
        let halt = report.outcome == super::super::model::TerminalOutcome::Partial
            && receipt.outcome == super::super::model::ItemOutcome::Partial;
        if receipt.outcome == super::super::model::ItemOutcome::Partial {
            retry = report.retry;
        }
        receipts.push(receipt);
        if halt {
            receipts.extend(sources[index + 1..].iter().enumerate().map(|(offset, _)| {
                super::super::model::ItemReceipt {
                    item: ItemId((index + 1 + offset) as u32),
                    outcome: super::super::model::ItemOutcome::CancelledBeforeStart,
                    detail: Some("stopped because cleanup or recovery is required".into()),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                }
            }));
            break;
        }
    }
    let completed = receipts.iter().any(|receipt| {
        matches!(
            receipt.outcome,
            super::super::model::ItemOutcome::Completed | super::super::model::ItemOutcome::Skipped
        )
    });
    let failed = receipts
        .iter()
        .any(|receipt| receipt.outcome == super::super::model::ItemOutcome::Failed);
    let partial = receipts
        .iter()
        .any(|receipt| receipt.outcome == super::super::model::ItemOutcome::Partial);
    let cancelled_items = receipts
        .iter()
        .any(|receipt| receipt.outcome == super::super::model::ItemOutcome::CancelledBeforeStart);
    let terminal_outcome = if cancelled_active && !completed && !failed {
        super::super::model::TerminalOutcome::Cancelled
    } else if partial
        || (failed && completed)
        || (cancelled_items && completed)
        || (cancelled_items && failed)
    {
        super::super::model::TerminalOutcome::Partial
    } else if failed {
        super::super::model::TerminalOutcome::Failed
    } else if cancelled_items {
        super::super::model::TerminalOutcome::Cancelled
    } else {
        super::super::model::TerminalOutcome::Succeeded
    };
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: terminal_outcome,
            final_counters: counters,
            receipts,
            retry,
            change_hints,
        },
    });
}

fn reject_virtual_batch(
    token: AttemptToken,
    sources: &[super::super::virtual_transfer::VirtualFileSource],
    emit: &mut impl FnMut(ExecutorFact),
) {
    let total_bytes = sources
        .iter()
        .try_fold(0_u64, |total, source| total.checked_add(source.size?));
    let discovered_bytes = sources
        .iter()
        .filter_map(|source| source.size)
        .fold(0_u64, u64::saturating_add);
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: TerminalOutcome::Failed,
            final_counters: super::super::model::ProgressCounters {
                discovered_items: sources.len() as u64,
                total_items: Some(sources.len() as u64),
                discovered_bytes,
                total_bytes,
                ..Default::default()
            },
            receipts: sources
                .iter()
                .enumerate()
                .map(|(index, _)| ItemReceipt {
                    item: ItemId(index as u32),
                    outcome: super::super::model::ItemOutcome::Failed,
                    detail: Some("virtual copy destination validation failed".into()),
                    created_folder_path: None,
                    created_archive_path: None,
                    move_receipt: None,
                    replace_receipt: None,
                    copy_undo: None,
                })
                .collect(),
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        },
    });
}

fn execute_virtual_copy_item(
    token: AttemptToken,
    parent: &Path,
    item: ItemId,
    source: super::super::virtual_transfer::VirtualFileSource,
    cancelled: &AtomicBool,
    controls: &Receiver<WorkerControl>,
    remaining_choice: &mut Option<super::super::model::ConflictDecision>,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let destination = parent.join(&source.name);
    let stage = parent.join(format!(
        ".gpui-filer-copy-{}-{}-{}-{}-virtual",
        std::process::id(),
        token.job_id().value(),
        token.attempt_id().value(),
        item.0
    ));
    let mut staged = match create_virtual_stage(&stage) {
        Ok(file) => file,
        Err(error) => {
            finish_virtual_failure(
                token,
                item,
                format!("could not create operation stage: {error:?}"),
                emit,
            );
            return;
        }
    };
    let mut copied = 0_u64;
    let mut sequence = 0_u64;
    let mut counters = ProgressCounters {
        total_items: Some(1),
        total_bytes: source.size,
        ..ProgressCounters::default()
    };
    let transfer_result = (|| -> Result<(), String> {
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            let chunk = super::super::virtual_transfer::read_chunk(source.token, CHUNK)
                .map_err(|error| error.to_string())?;
            if chunk.is_empty() {
                break;
            }
            if cancelled.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            staged
                .write_all(&chunk)
                .map_err(|error| error.to_string())?;
            copied = copied.saturating_add(chunk.len() as u64);
            counters.processed_bytes = copied;
            sequence = sequence.saturating_add(1);
            emit(ExecutorFact::Progress {
                token,
                update: ProgressUpdate {
                    sequence,
                    counters,
                    current_item: Some(item),
                },
            });
        }
        if source.size.is_some_and(|expected| expected != copied) {
            return Err("virtual stream length did not match its descriptor".into());
        }
        staged.flush().map_err(|error| error.to_string())?;
        staged.sync_all().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(error) = transfer_result {
        let cleanup = cleanup_virtual_stage(&staged, &stage);
        drop(staged);
        if let Err(cleanup_error) = cleanup {
            finish_virtual_cleanup_failure(token, item, error, cleanup_error, emit);
            return;
        }
        if error == "cancelled" {
            emit(ExecutorFact::Terminal {
                token,
                report: TerminalReport {
                    outcome: TerminalOutcome::Cancelled,
                    final_counters: counters,
                    receipts: vec![ItemReceipt {
                        item,
                        outcome: ItemOutcome::CancelledBeforeStart,
                        detail: Some("cancelled before destination publication".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    }],
                    retry: RetryPlan::NotAllowed,
                    change_hints: Vec::new(),
                },
            });
        } else {
            finish_virtual_failure(token, item, error, emit);
        }
        return;
    }

    let mut keep_both = false;
    let mut probe_revision = 0_u64;
    loop {
        if cancelled.load(Ordering::Acquire) {
            let cleanup = cleanup_virtual_stage(&staged, &stage);
            drop(staged);
            if let Err(cleanup_error) = cleanup {
                finish_virtual_cleanup_failure(
                    token,
                    item,
                    "cancelled".into(),
                    cleanup_error,
                    emit,
                );
                return;
            }
            emit(ExecutorFact::Terminal {
                token,
                report: TerminalReport {
                    outcome: TerminalOutcome::Cancelled,
                    final_counters: counters,
                    receipts: vec![ItemReceipt {
                        item,
                        outcome: ItemOutcome::CancelledBeforeStart,
                        detail: Some("cancelled before destination publication".into()),
                        created_folder_path: None,
                        created_archive_path: None,
                        move_receipt: None,
                        replace_receipt: None,
                        copy_undo: None,
                    }],
                    retry: RetryPlan::NotAllowed,
                    change_hints: Vec::new(),
                },
            });
            return;
        }
        let target = destination.clone();
        if !keep_both {
            match std::fs::symlink_metadata(&target) {
                Ok(_) => {
                    match *remaining_choice {
                        Some(super::super::model::ConflictDecision::Skip) => {
                            finish_virtual_skipped(token, item, counters, &staged, &stage, emit);
                            return;
                        }
                        Some(super::super::model::ConflictDecision::KeepBoth) => {
                            keep_both = true;
                            continue;
                        }
                        _ => {}
                    }
                    probe_revision = probe_revision.saturating_add(1);
                    emit(ExecutorFact::Conflict {
                        token,
                        notice: super::super::model::ConflictNotice {
                            item,
                            probe_revision,
                            description: format!("{} already exists", target.display()),
                            replace_supported: false,
                            directory_move_replace_rename_candidate: None,
                        },
                    });
                    let decision = loop {
                        if cancelled.load(Ordering::Acquire) {
                            break None;
                        }
                        match controls.recv_timeout(Duration::from_millis(50)) {
                            Ok(WorkerControl::Resolve {
                                decision,
                                apply_to_remaining,
                            }) => {
                                if apply_to_remaining {
                                    *remaining_choice = Some(decision);
                                }
                                break Some(decision);
                            }
                            Ok(WorkerControl::Cancel)
                            | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
                            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        }
                    };
                    if cancelled.load(Ordering::Acquire) {
                        let cleanup = cleanup_virtual_stage(&staged, &stage);
                        drop(staged);
                        if let Err(cleanup_error) = cleanup {
                            finish_virtual_cleanup_failure(
                                token,
                                item,
                                "cancelled while resolving conflict".into(),
                                cleanup_error,
                                emit,
                            );
                        } else {
                            finish_virtual_cancelled(token, item, counters, emit);
                        }
                        return;
                    }
                    match decision {
                        Some(super::super::model::ConflictDecision::Skip) => {
                            let cleanup = cleanup_virtual_stage(&staged, &stage);
                            drop(staged);
                            if let Err(cleanup_error) = cleanup {
                                finish_virtual_cleanup_failure(
                                    token,
                                    item,
                                    "Skip selected".into(),
                                    cleanup_error,
                                    emit,
                                );
                                return;
                            }
                            emit(ExecutorFact::Terminal {
                                token,
                                report: TerminalReport {
                                    outcome: TerminalOutcome::Succeeded,
                                    final_counters: counters,
                                    receipts: vec![ItemReceipt {
                                        item,
                                        outcome: ItemOutcome::Skipped,
                                        detail: None,
                                        created_folder_path: None,
                                        created_archive_path: None,
                                        move_receipt: None,
                                        replace_receipt: None,
                                        copy_undo: None,
                                    }],
                                    retry: RetryPlan::NotAllowed,
                                    change_hints: Vec::new(),
                                },
                            });
                            return;
                        }
                        Some(super::super::model::ConflictDecision::KeepBoth) => keep_both = true,
                        _ => {
                            let cleanup = cleanup_virtual_stage(&staged, &stage);
                            drop(staged);
                            if let Err(cleanup_error) = cleanup {
                                finish_virtual_cleanup_failure(
                                    token,
                                    item,
                                    "conflict cancelled".into(),
                                    cleanup_error,
                                    emit,
                                );
                                return;
                            }
                            emit(ExecutorFact::Terminal {
                                token,
                                report: TerminalReport {
                                    outcome: TerminalOutcome::Cancelled,
                                    final_counters: counters,
                                    receipts: vec![ItemReceipt {
                                        item,
                                        outcome: ItemOutcome::CancelledBeforeStart,
                                        detail: Some(
                                            "cancelled before destination publication".into(),
                                        ),
                                        created_folder_path: None,
                                        created_archive_path: None,
                                        move_receipt: None,
                                        replace_receipt: None,
                                        copy_undo: None,
                                    }],
                                    retry: RetryPlan::NotAllowed,
                                    change_hints: Vec::new(),
                                },
                            });
                            return;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    let cleanup = cleanup_virtual_stage(&staged, &stage);
                    drop(staged);
                    if let Err(cleanup_error) = cleanup {
                        finish_virtual_cleanup_failure(
                            token,
                            item,
                            error.to_string(),
                            cleanup_error,
                            emit,
                        );
                    } else {
                        finish_virtual_failure(token, item, error.to_string(), emit);
                    }
                    return;
                }
            }
        }
        if keep_both {
            let mut published = false;
            for suffix in 2..10_000 {
                if cancelled.load(Ordering::Acquire) {
                    let cleanup = cleanup_virtual_stage(&staged, &stage);
                    drop(staged);
                    if let Err(cleanup_error) = cleanup {
                        finish_virtual_cleanup_failure(
                            token,
                            item,
                            "cancelled before Keep Both publication".into(),
                            cleanup_error,
                            emit,
                        );
                    } else {
                        finish_virtual_cancelled(token, item, counters, emit);
                    }
                    return;
                }
                let Some(candidate) =
                    super::super::model::keep_both_candidate_path(&destination, suffix)
                else {
                    break;
                };
                match publish_virtual_stage(&staged, &stage, &candidate) {
                    Ok(()) => {
                        published = true;
                        break;
                    }
                    Err(_) if target_occupied(&candidate) => continue,
                    Err(error) => {
                        let cleanup = cleanup_virtual_stage(&staged, &stage);
                        drop(staged);
                        if let Err(cleanup_error) = cleanup {
                            finish_virtual_cleanup_failure(
                                token,
                                item,
                                format!("{error:?}"),
                                cleanup_error,
                                emit,
                            );
                        } else {
                            finish_virtual_failure(token, item, format!("{error:?}"), emit);
                        }
                        return;
                    }
                }
            }
            if !published {
                let cleanup = cleanup_virtual_stage(&staged, &stage);
                drop(staged);
                if let Err(cleanup_error) = cleanup {
                    finish_virtual_cleanup_failure(
                        token,
                        item,
                        "no free Keep Both name was available".into(),
                        cleanup_error,
                        emit,
                    );
                    return;
                }
                finish_virtual_failure(
                    token,
                    item,
                    "no free Keep Both name was available".into(),
                    emit,
                );
                return;
            }
        } else {
            if cancelled.load(Ordering::Acquire) {
                let cleanup = cleanup_virtual_stage(&staged, &stage);
                drop(staged);
                if let Err(cleanup_error) = cleanup {
                    finish_virtual_cleanup_failure(
                        token,
                        item,
                        "cancelled before publication".into(),
                        cleanup_error,
                        emit,
                    );
                } else {
                    finish_virtual_cancelled(token, item, counters, emit);
                }
                return;
            }
            match publish_virtual_stage(&staged, &stage, &target) {
                Ok(()) => {}
                Err(_) if target_occupied(&target) => continue,
                Err(error) => {
                    let cleanup = cleanup_virtual_stage(&staged, &stage);
                    drop(staged);
                    if let Err(cleanup_error) = cleanup {
                        finish_virtual_cleanup_failure(
                            token,
                            item,
                            format!("{error:?}"),
                            cleanup_error,
                            emit,
                        );
                    } else {
                        finish_virtual_failure(token, item, format!("{error:?}"), emit);
                    }
                    return;
                }
            }
        }
        drop(staged);
        counters.processed_items = 1;
        emit(ExecutorFact::Terminal {
            token,
            report: TerminalReport {
                outcome: TerminalOutcome::Succeeded,
                final_counters: counters,
                receipts: vec![ItemReceipt {
                    item,
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
                    directory: parent.to_path_buf(),
                }],
            },
        });
        return;
    }
}

fn target_occupied(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn create_virtual_stage(path: &Path) -> Result<std::fs::File, CopyError> {
    #[cfg(windows)]
    {
        windows::create_virtual_stage(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(io)
    }
}

fn publish_virtual_stage(
    stage_file: &std::fs::File,
    stage_path: &Path,
    destination: &Path,
) -> Result<(), CopyError> {
    #[cfg(windows)]
    {
        let _ = stage_path;
        windows::publish_no_replace(stage_file, destination)
    }
    #[cfg(not(windows))]
    {
        std::fs::hard_link(stage_path, destination).map_err(io)?;
        std::fs::remove_file(stage_path).map_err(io)
    }
}

fn finish_virtual_failure(
    token: AttemptToken,
    item: ItemId,
    detail: String,
    emit: &mut impl FnMut(ExecutorFact),
) {
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: TerminalOutcome::Failed,
            final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt {
                item,
                outcome: ItemOutcome::Failed,
                detail: Some(detail),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        },
    });
}

fn finish_virtual_cancelled(
    token: AttemptToken,
    item: ItemId,
    counters: ProgressCounters,
    emit: &mut impl FnMut(ExecutorFact),
) {
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: TerminalOutcome::Cancelled,
            final_counters: counters,
            receipts: vec![ItemReceipt {
                item,
                outcome: ItemOutcome::CancelledBeforeStart,
                detail: Some("cancelled before destination publication".into()),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        },
    });
}

fn finish_virtual_skipped(
    token: AttemptToken,
    item: ItemId,
    counters: ProgressCounters,
    staged: &std::fs::File,
    stage: &Path,
    emit: &mut impl FnMut(ExecutorFact),
) {
    if let Err(error) = cleanup_virtual_stage(staged, stage) {
        finish_virtual_cleanup_failure(token, item, "Skip selected".into(), error, emit);
        return;
    }
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: TerminalOutcome::Succeeded,
            final_counters: counters,
            receipts: vec![ItemReceipt {
                item,
                outcome: ItemOutcome::Skipped,
                detail: None,
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        },
    });
}

fn finish_virtual_cleanup_failure(
    token: AttemptToken,
    item: ItemId,
    cause: String,
    cleanup: CopyError,
    emit: &mut impl FnMut(ExecutorFact),
) {
    let detail = format!("{cause}; exact staged object cleanup failed: {cleanup:?}");
    emit(ExecutorFact::Terminal {
        token,
        report: TerminalReport {
            outcome: TerminalOutcome::Partial,
            final_counters: ProgressCounters::default(),
            receipts: vec![ItemReceipt {
                item,
                outcome: ItemOutcome::Partial,
                detail: Some(detail),
                created_folder_path: None,
                created_archive_path: None,
                move_receipt: None,
                replace_receipt: None,
                copy_undo: None,
            }],
            retry: RetryPlan::NotAllowed,
            change_hints: Vec::new(),
        },
    });
}

fn cleanup_virtual_stage(file: &std::fs::File, path: &Path) -> Result<(), CopyError> {
    #[cfg(windows)]
    {
        let _ = path;
        windows::cleanup_owned(file)
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        std::fs::remove_file(path).map_err(io)
    }
}
