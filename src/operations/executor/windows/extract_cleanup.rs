//! Extract7z cleanup keeps native ownership explicit across disposition, close,
//! and an ID-bearing rescan of the retained parent directory.

use super::*;
use crate::operations::journal::JournalSevenZipExtractIdentity;
use ::windows::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
    Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FILE_ID_BOTH_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS,
        FileDispositionInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        GetFileInformationByHandleEx, SetFileInformationByHandle,
    },
};
use std::{
    ffi::c_void,
    fmt,
    fs::File,
    mem::{ManuallyDrop, offset_of, size_of},
    os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle},
    path::Path,
    time::{Duration, Instant},
};

const SCAN_ENTRY_LIMIT: usize = 65_536;
const SCAN_NAME_BYTES_LIMIT: usize = 8 * 1024 * 1024;
const CLEANUP_BUDGET: Duration = Duration::from_secs(30);

fn wait_until_deadline(deadline: Instant) {
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Debug)]
pub(in crate::operations::executor) enum SpoolDeleteFailure {
    Retained {
        file: File,
        identity: Option<JournalSevenZipExtractIdentity>,
        disposition_accepted: bool,
        reason: &'static str,
    },
    CloseUncertain {
        raw: HANDLE,
        identity: JournalSevenZipExtractIdentity,
    },
    AwaitingAbsence {
        identity: JournalSevenZipExtractIdentity,
        reason: &'static str,
    },
}

/// Caller must hold the exact parent route and exact DeleteIntent ACK. A clean
/// fact is returned only after the old ID is absent from every directory page.
pub(in crate::operations::executor) fn delete_spool(
    file: File,
    expected_identity: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    expected_parent: &JournalSevenZipExtractIdentity,
    deadline: Instant,
) -> Result<(), SpoolDeleteFailure> {
    delete_spool_inner(
        file,
        expected_identity,
        parent_path,
        expected_parent,
        deadline,
        false,
        None,
    )
}

fn delete_spool_inner(
    file: File,
    expected_identity: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    expected_parent: &JournalSevenZipExtractIdentity,
    deadline: Instant,
    disposition_accepted: bool,
    fault: Option<CleanupTestFault>,
) -> Result<(), SpoolDeleteFailure> {
    if Instant::now() >= deadline {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: None,
            disposition_accepted,
            reason: "spool cleanup deadline expired before identity query",
        });
    }
    if matches!(fault, Some(CleanupTestFault::SpoolIdentity)) {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: None,
            disposition_accepted,
            reason: "injected spool identity query failure",
        });
    }
    let actual = match super::regular_file_identity(&file) {
        Ok(id) => JournalSevenZipExtractIdentity {
            volume: id.volume,
            file_index: (u64::from(id.index_high) << 32) | u64::from(id.index_low),
        },
        Err(_) => {
            return Err(SpoolDeleteFailure::Retained {
                file,
                identity: None,
                disposition_accepted,
                reason: "spool identity query failed",
            });
        }
    };
    if &actual != expected_identity {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "spool identity changed",
        });
    }
    if super::ensure_local_fixed_ntfs(parent_path).is_err() {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "spool parent is outside local fixed NTFS scope",
        });
    }
    if Instant::now() >= deadline {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "spool cleanup deadline expired before parent query",
        });
    }
    if matches!(fault, Some(CleanupTestFault::SpoolParent)) {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "injected spool parent query failure",
        });
    }
    let parent = match super::open_staging_directory(parent_path) {
        Ok(parent) => parent,
        Err(_) => {
            return Err(SpoolDeleteFailure::Retained {
                file,
                identity: Some(actual),
                disposition_accepted,
                reason: "spool parent could not be pinned",
            });
        }
    };
    if !parent_matches(&parent, expected_parent) {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "spool parent identity changed",
        });
    }
    if Instant::now() >= deadline {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "spool cleanup deadline expired before disposition",
        });
    }
    if matches!(fault, Some(CleanupTestFault::SpoolDisposition)) {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "injected spool disposition failure",
        });
    }
    let disposition_accepted = if disposition_accepted {
        true
    } else {
        let info = FILE_DISPOSITION_INFO {
            DeleteFile: ::windows::Win32::Foundation::BOOLEAN(1),
        };
        if unsafe {
            SetFileInformationByHandle(
                super::raw(&file),
                FileDispositionInfo,
                &info as *const _ as *const c_void,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .is_err()
        {
            return Err(SpoolDeleteFailure::Retained {
                file,
                identity: Some(actual),
                disposition_accepted: false,
                reason: "spool disposition was not accepted",
            });
        }
        true
    };
    if Instant::now() >= deadline {
        return Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(actual),
            disposition_accepted,
            reason: "spool cleanup deadline expired before checked close",
        });
    }
    let raw = file.into_raw_handle();
    if matches!(fault, Some(CleanupTestFault::SpoolClose)) {
        return Err(SpoolDeleteFailure::CloseUncertain {
            raw: HANDLE(raw.cast()),
            identity: actual,
        });
    }
    if unsafe { CloseHandle(HANDLE(raw.cast())) }.is_err() {
        return Err(SpoolDeleteFailure::CloseUncertain {
            raw: HANDLE(raw.cast()),
            identity: actual,
        });
    }
    if matches!(fault, Some(CleanupTestFault::SpoolAbsence)) || Instant::now() >= deadline {
        return Err(SpoolDeleteFailure::AwaitingAbsence {
            identity: actual,
            reason: "spool absence confirmation unavailable",
        });
    }
    match parent_absence(&parent, expected_parent, &actual, deadline) {
        Ok(true) => Ok(()),
        Ok(false) => Err(SpoolDeleteFailure::AwaitingAbsence {
            identity: actual,
            reason: "old spool ID remains in parent enumeration",
        }),
        Err(()) => Err(SpoolDeleteFailure::AwaitingAbsence {
            identity: actual,
            reason: "spool absence enumeration was inconclusive",
        }),
    }
}

pub(in crate::operations::executor) fn resume_spool_delete(
    failure: SpoolDeleteFailure,
    expected_identity: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    expected_parent: &JournalSevenZipExtractIdentity,
    deadline: Instant,
) -> Result<(), SpoolDeleteFailure> {
    match failure {
        SpoolDeleteFailure::Retained {
            file,
            identity: Some(identity),
            disposition_accepted,
            ..
        } if &identity == expected_identity => delete_spool_inner(
            file,
            &identity,
            parent_path,
            expected_parent,
            deadline,
            disposition_accepted,
            None,
        ),
        SpoolDeleteFailure::Retained {
            file,
            identity: None,
            ..
        } => delete_spool_inner(
            file,
            expected_identity,
            parent_path,
            expected_parent,
            deadline,
            false,
            None,
        ),
        SpoolDeleteFailure::Retained {
            file,
            identity: Some(identity),
            disposition_accepted,
            ..
        } => Err(SpoolDeleteFailure::Retained {
            file,
            identity: Some(identity),
            disposition_accepted,
            reason: "spool identity differs from retry expectation",
        }),
        failure @ SpoolDeleteFailure::CloseUncertain { .. } => Err(failure),
        SpoolDeleteFailure::AwaitingAbsence { identity, reason } => {
            match confirm_spool_absence(&identity, parent_path, expected_parent, deadline) {
                Ok(true) => Ok(()),
                _ => Err(SpoolDeleteFailure::AwaitingAbsence { identity, reason }),
            }
        }
    }
}

pub(in crate::operations::executor) fn confirm_spool_absence(
    identity: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    expected_parent: &JournalSevenZipExtractIdentity,
    deadline: Instant,
) -> Result<bool, ()> {
    super::ensure_local_fixed_ntfs(parent_path).map_err(|_| ())?;
    let parent = super::open_staging_directory(parent_path).map_err(|_| ())?;
    parent_absence(&parent, expected_parent, identity, deadline)
}

pub(in crate::operations::executor) enum StagedTreeDeleteSlot {
    Held {
        node: CleanupNode,
        disposition_accepted: bool,
    },
    ClosedAwaitingAbsence {
        relative_components: Vec<Vec<u16>>,
        identity: String,
    },
    CloseUncertain {
        relative_components: Vec<Vec<u16>>,
        raw: HANDLE,
        identity: String,
    },
    Removed {
        relative_components: Vec<Vec<u16>>,
        identity: String,
    },
}

pub(in crate::operations::executor) struct CleanupNode {
    relative_components: Vec<Vec<u16>>,
    node_kind: crate::operations::journal::JournalDirectoryNodeKind,
    identity: Option<String>,
    handle: super::PinnedSourceHandle,
}

#[derive(Clone, Copy)]
enum CleanupTestFault {
    ChildIdentity,
    ChildIdentityAt(usize),
    DirectoryEmpty,
    DirectoryEmptyAt(usize),
    ChildDisposition,
    ChildDispositionAt(usize),
    ChildClose,
    RootIdentity,
    RootEmpty,
    RootDisposition,
    RootClose,
    WorkerRootClose,
    ExpireAfterRootIdentity,
    ExpireAfterRootEmpty,
    ExpireAfterRootDisposition,
    ExpireAfterChildIdentity(usize),
    ExpireAfterChildEmpty(usize),
    ExpireAfterChildDisposition(usize),
    RootAbsence,
    SpoolIdentity,
    SpoolParent,
    SpoolDisposition,
    SpoolClose,
    SpoolAbsence,
}

#[cfg(test)]
static WORKER_CLOSE_UNCERTAIN_HANDLE: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);

pub(in crate::operations::executor) struct StagedTreeDeleteFailure {
    pub(in crate::operations::executor) root: Option<super::StagingRootLease>,
    pub(in crate::operations::executor) root_disposition_accepted: bool,
    pub(in crate::operations::executor) root_chain: Vec<DirectoryLock>,
    pub(in crate::operations::executor) root_close_uncertain: Option<(HANDLE, String)>,
    pub(in crate::operations::executor) root_closed_awaiting_absence: Option<String>,
    pub(in crate::operations::executor) slots: Vec<StagedTreeDeleteSlot>,
    pub(in crate::operations::executor) reason: &'static str,
}

#[derive(Debug)]
pub(in crate::operations::executor) enum RetainedRootDeleteFailure {
    IdentityUncertain {
        root: super::retained_tree::RetainedRoot,
        reason: &'static str,
    },
    Cleanup(StagedTreeDeleteFailure),
}

impl fmt::Debug for StagedTreeDeleteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedTreeDeleteFailure")
            .field("root_retained", &self.root.is_some())
            .field("root_close_uncertain", &self.root_close_uncertain.is_some())
            .field(
                "root_absence_pending",
                &self.root_closed_awaiting_absence.is_some(),
            )
            .field("slot_count", &self.slots.len())
            .field("reason", &self.reason)
            .finish()
    }
}

impl StagedTreeDeleteFailure {
    /// Move all known owned handles into Send-safe ownership and keep uncertain
    /// raw values opaque. The latter are never reconstructed or closed.
    pub(in crate::operations::executor) fn into_passive_undo_custody(
        self,
    ) -> (super::DirectoryCopyUndoCustody, bool, bool) {
        fn hold_directory(lock: DirectoryLock, held: &mut Vec<OwnedHandle>) {
            let lock = ManuallyDrop::new(lock);
            let raw = lock.0.0;
            // The DirectoryLock still owns this exact valid handle; convert
            // that ownership into the standard Send-safe owner exactly once.
            held.push(unsafe { OwnedHandle::from_raw_handle(raw) });
        }

        fn hold_node(handle: super::PinnedSourceHandle, held: &mut Vec<OwnedHandle>) {
            match handle {
                super::PinnedSourceHandle::Directory(directory) => hold_directory(directory, held),
                super::PinnedSourceHandle::File(file) => held.push(file.into()),
            }
        }

        let mut owned_handles = Vec::new();
        let mut opaque_uncertain_handles = Vec::new();
        let root_disposition_accepted = self.root_disposition_accepted;
        let root_close_uncertain = self.root_close_uncertain.is_some();
        let mut accepted_disposition = root_disposition_accepted
            || self.root_closed_awaiting_absence.is_some();
        if let Some((raw, _identity)) = self.root_close_uncertain {
            opaque_uncertain_handles.push(raw.0 as usize);
        }
        if let Some(root) = self.root {
            let super::StagingRootLease { root_chain, root, .. } = root;
            for ancestor in root_chain { hold_directory(ancestor, &mut owned_handles); }
            hold_directory(root, &mut owned_handles);
        }
        for ancestor in self.root_chain { hold_directory(ancestor, &mut owned_handles); }
        for slot in self.slots {
            match slot {
                StagedTreeDeleteSlot::Held { node, disposition_accepted } => {
                    accepted_disposition |= disposition_accepted;
                    hold_node(node.handle, &mut owned_handles);
                }
                StagedTreeDeleteSlot::CloseUncertain { raw, .. } => {
                    accepted_disposition = true;
                    opaque_uncertain_handles.push(raw.0 as usize);
                }
                StagedTreeDeleteSlot::ClosedAwaitingAbsence { .. }
                | StagedTreeDeleteSlot::Removed { .. } => accepted_disposition = true,
            }
        }
        let uncertain = root_close_uncertain || !opaque_uncertain_handles.is_empty();
        (
            super::DirectoryCopyUndoCustody {
                _owned_handles: owned_handles,
                _opaque_uncertain_handles: opaque_uncertain_handles,
            },
            accepted_disposition,
            uncertain,
        )
    }
}

/// Consumes only the exact unpublished stage. Every slot remains represented
/// after a failure so a caller may retain facts without rediscovering names.
pub(in crate::operations::executor) fn delete_staged_tree(
    staged: super::VerifiedStagedTree,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
) -> Result<(), StagedTreeDeleteFailure> {
    delete_staged_tree_with_fault(staged, expected_parent, parent_path, deadline, None, None)
}

#[cfg(test)]
pub(in crate::operations::executor) fn delete_staged_tree_close_fault_for_test(
    staged: super::VerifiedStagedTree,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
    root: bool,
    uncertain_handle_capture: &std::sync::atomic::AtomicIsize,
) -> Result<(), StagedTreeDeleteFailure> {
    delete_staged_tree_with_fault(
        staged,
        expected_parent,
        parent_path,
        deadline,
        Some(if root { CleanupTestFault::WorkerRootClose } else { CleanupTestFault::ChildClose }),
        Some(uncertain_handle_capture),
    )
}

fn delete_staged_tree_with_fault(
    staged: super::VerifiedStagedTree,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
    fault: Option<CleanupTestFault>,
    uncertain_handle_capture: Option<&std::sync::atomic::AtomicIsize>,
) -> Result<(), StagedTreeDeleteFailure> {
    let super::VerifiedStagedTree {
        root_path,
        root_identity,
        root_chain,
        root,
        handles,
        ..
    } = staged;
    let mut slots = handles
        .into_iter()
        .map(|node| StagedTreeDeleteSlot::Held {
            node: CleanupNode {
                relative_components: node.relative_components,
                node_kind: node.node_kind,
                identity: Some(node.identity),
                handle: node.handle,
            },
            disposition_accepted: false,
        })
        .collect::<Vec<_>>();
    slots.sort_by_key(|slot| {
        std::cmp::Reverse(slot_components(slot).map_or(0, |components| components.len()))
    });
    let mut root = Some(super::StagingRootLease {
        root_path,
        root_identity,
        root_chain,
        root,
    });
    delete_staged_tree_custody(
        root.take(),
        slots,
        expected_parent,
        parent_path,
        deadline,
        fault,
        uncertain_handle_capture,
        false,
    )
}

pub(in crate::operations::executor) fn delete_retained_spool_tree(
    retained: super::spool_tree::RetainedSpoolTree,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
) -> Result<(), StagedTreeDeleteFailure> {
    let (root, children) = super::spool_tree::into_cleanup_projection(retained);
    let mut slots = children
        .into_iter()
        .map(|child| StagedTreeDeleteSlot::Held {
            node: CleanupNode {
                relative_components: child.relative_components,
                node_kind: child.node_kind,
                identity: child.observed_identity,
                handle: child.handle,
            },
            disposition_accepted: false,
        })
        .collect::<Vec<_>>();
    slots.sort_by_key(|slot| {
        std::cmp::Reverse(slot_components(slot).map_or(0, |components| components.len()))
    });
    delete_staged_tree_custody(
        Some(root),
        slots,
        expected_parent,
        parent_path,
        deadline,
        None,
        None,
        false,
    )
}

#[cfg(test)]
pub(in crate::operations::executor) fn delete_retained_spool_tree_root_close_uncertain_for_test(
    retained: super::spool_tree::RetainedSpoolTree,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
) -> Result<(), StagedTreeDeleteFailure> {
    let (root, children) = super::spool_tree::into_cleanup_projection(retained);
    let mut slots = children
        .into_iter()
        .map(|child| StagedTreeDeleteSlot::Held {
            node: CleanupNode {
                relative_components: child.relative_components,
                node_kind: child.node_kind,
                identity: child.observed_identity,
                handle: child.handle,
            },
            disposition_accepted: false,
        })
        .collect::<Vec<_>>();
    slots.sort_by_key(|slot| {
        std::cmp::Reverse(slot_components(slot).map_or(0, |components| components.len()))
    });
    delete_staged_tree_custody(
        Some(root),
        slots,
        expected_parent,
        parent_path,
        deadline,
        Some(CleanupTestFault::WorkerRootClose),
        None,
        false,
    )
}

#[cfg(test)]
// 実際のidentity query後に期限を切り、期限切れ後のroot mutationを通さないことを検証する。
pub(in crate::operations::executor) fn delete_retained_spool_tree_expire_after_root_identity_for_test(
    retained: super::spool_tree::RetainedSpoolTree,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
) -> Result<(), StagedTreeDeleteFailure> {
    let (root, children) = super::spool_tree::into_cleanup_projection(retained);
    let mut slots = children
        .into_iter()
        .map(|child| StagedTreeDeleteSlot::Held {
            node: CleanupNode {
                relative_components: child.relative_components,
                node_kind: child.node_kind,
                identity: child.observed_identity,
                handle: child.handle,
            },
            disposition_accepted: false,
        })
        .collect::<Vec<_>>();
    slots.sort_by_key(|slot| {
        std::cmp::Reverse(slot_components(slot).map_or(0, |components| components.len()))
    });
    delete_staged_tree_custody(
        Some(root),
        slots,
        expected_parent,
        parent_path,
        deadline,
        Some(CleanupTestFault::ExpireAfterRootIdentity),
        None,
        false,
    )
}

#[cfg(test)]
pub(in crate::operations::executor) fn close_worker_quarantined_handle_after_terminal_for_test()
-> bool {
    let raw = WORKER_CLOSE_UNCERTAIN_HANDLE.swap(0, std::sync::atomic::Ordering::AcqRel);
    raw != 0 && unsafe { CloseHandle(HANDLE(raw as *mut _)) }.is_ok()
}

#[cfg(test)]
pub(in crate::operations::executor) fn close_captured_uncertain_handle_after_terminal_for_test(
    capture: &std::sync::atomic::AtomicIsize,
) -> bool {
    let raw = capture.swap(0, std::sync::atomic::Ordering::AcqRel);
    raw != 0 && unsafe { CloseHandle(HANDLE(raw as *mut _)) }.is_ok()
}

pub(in crate::operations::executor) fn delete_retained_root(
    retained: super::retained_tree::RetainedRoot,
    expected_root_identity: &str,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
) -> Result<(), RetainedRootDeleteFailure> {
    let actual = match super::directory_identity_text(&retained.root) {
        Ok(actual) => actual,
        Err(_) => {
            return Err(RetainedRootDeleteFailure::IdentityUncertain {
                root: retained,
                reason: "retained root identity could not be observed",
            });
        }
    };
    if actual != expected_root_identity {
        return Err(RetainedRootDeleteFailure::IdentityUncertain {
            root: retained,
            reason: "retained root identity differs from acknowledged identity",
        });
    }
    delete_staged_tree_custody(
        Some(super::StagingRootLease {
            root_path: retained.root_path,
            root_identity: actual,
            root_chain: retained.root_chain,
            root: retained.root,
        }),
        Vec::new(),
        expected_parent,
        parent_path,
        deadline,
        None,
        None,
        false,
    )
    .map_err(RetainedRootDeleteFailure::Cleanup)
}

pub(in crate::operations::executor) fn resume_staged_tree_delete(
    mut failure: StagedTreeDeleteFailure,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
) -> Result<(), StagedTreeDeleteFailure> {
    if let Some(root) = failure.root.take() {
        return delete_staged_tree_custody(
            Some(root),
            failure.slots,
            expected_parent,
            parent_path,
            deadline,
            None,
            None,
            failure.root_disposition_accepted,
        );
    }
    // checked close 後は名前再利用ではなく、旧 ID が消えた事実だけを再確認する。
    if let Some(identity) = failure.root_closed_awaiting_absence.take() {
        let Ok(directory) = super::open_staging_directory(parent_path) else {
            failure.root_closed_awaiting_absence = Some(identity);
            return Err(failure);
        };
        let Some(file_index) = parse_identity(&identity) else {
            failure.root_closed_awaiting_absence = Some(identity);
            return Err(failure);
        };
        return match parent_absence(
            &directory,
            expected_parent,
            &JournalSevenZipExtractIdentity {
                volume: expected_parent.volume,
                file_index,
            },
            deadline,
        ) {
            Ok(true) => Ok(()),
            _ => {
                failure.root_closed_awaiting_absence = Some(identity);
                Err(failure)
            }
        };
    }
    // CloseUncertain の raw handle は二度 close しない。caller は未知 custody を保持する。
    Err(failure)
}

fn delete_staged_tree_custody(
    mut root: Option<super::StagingRootLease>,
    mut slots: Vec<StagedTreeDeleteSlot>,
    expected_parent: &JournalSevenZipExtractIdentity,
    parent_path: &Path,
    deadline: Instant,
    mut fault: Option<CleanupTestFault>,
    uncertain_handle_capture: Option<&std::sync::atomic::AtomicIsize>,
    mut root_disposition_accepted: bool,
) -> Result<(), StagedTreeDeleteFailure> {
    if super::ensure_local_fixed_ntfs(parent_path).is_err() {
        return Err(StagedTreeDeleteFailure {
            root,
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging parent is outside local fixed NTFS scope",
        });
    }
    if Instant::now() >= deadline {
        return Err(StagedTreeDeleteFailure {
            root,
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging cleanup deadline expired",
        });
    }
    let parent = match super::open_staging_directory(parent_path) {
        Ok(parent) if parent_matches(&parent, expected_parent) => parent,
        _ => {
            return Err(StagedTreeDeleteFailure {
                root,
                root_chain: Vec::new(),
                root_close_uncertain: None,
                root_disposition_accepted,
                root_closed_awaiting_absence: None,
                slots,
                reason: "staging parent identity changed",
            });
        }
    };
    for index in 0..slots.len() {
        if Instant::now() >= deadline {
            return Err(StagedTreeDeleteFailure {
                root,
                root_chain: Vec::new(),
                root_close_uncertain: None,
                root_disposition_accepted,
                root_closed_awaiting_absence: None,
                slots,
                reason: "staging cleanup deadline expired",
            });
        }
        if matches!(slots[index], StagedTreeDeleteSlot::Removed { .. }) {
            continue;
        }
        let result = delete_staged_slot(
            &mut slots,
            index,
            root.as_ref().unwrap(),
            deadline,
            &mut fault,
            uncertain_handle_capture,
        );
        if let Err(reason) = result {
            return Err(StagedTreeDeleteFailure {
                root,
                root_chain: Vec::new(),
                root_close_uncertain: None,
                root_disposition_accepted,
                root_closed_awaiting_absence: None,
                slots,
                reason,
            });
        }
    }
    if Instant::now() >= deadline {
        return Err(StagedTreeDeleteFailure {
            root,
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging cleanup deadline expired",
        });
    }
    let root_value = root
        .take()
        .expect("root custody retained until all children finish");
    if matches!(fault, Some(CleanupTestFault::RootIdentity)) {
        return Err(StagedTreeDeleteFailure {
            root: Some(root_value),
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "injected staging root identity query failure",
        });
    }
    let root_identity_matches = super::directory_identity_text(&root_value.root)
        .is_ok_and(|actual| actual == root_value.root_identity);
    if !root_identity_matches {
        return Err(StagedTreeDeleteFailure {
            root: Some(root_value),
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging root identity changed",
        });
    }
    if matches!(fault, Some(CleanupTestFault::ExpireAfterRootIdentity)) {
        wait_until_deadline(deadline);
    }
    if Instant::now() >= deadline {
        return Err(StagedTreeDeleteFailure {
            root: Some(root_value),
            root_disposition_accepted,
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging cleanup deadline expired after root identity query",
        });
    }
    if matches!(fault, Some(CleanupTestFault::RootEmpty)) {
        return Err(StagedTreeDeleteFailure {
            root: Some(root_value),
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "injected staging root empty-check failure",
        });
    }
    if !enumerate_empty(&root_value.root) {
        return Err(StagedTreeDeleteFailure {
            root: Some(root_value),
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging root contains an unowned child",
        });
    }
    if matches!(fault, Some(CleanupTestFault::ExpireAfterRootEmpty)) {
        wait_until_deadline(deadline);
    }
    if Instant::now() >= deadline {
        return Err(StagedTreeDeleteFailure {
            root: Some(root_value),
            root_disposition_accepted,
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging cleanup deadline expired after root empty query",
        });
    }
    let root_identity = root_value.root_identity.clone();
    let raw = raw_root(root_value.root);
    if !root_disposition_accepted && matches!(fault, Some(CleanupTestFault::RootDisposition)) {
        return Err(StagedTreeDeleteFailure {
            root: Some(super::StagingRootLease {
                root_path: root_value.root_path,
                root_identity: root_value.root_identity,
                root_chain: root_value.root_chain,
                root: DirectoryLock(raw),
            }),
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted: false,
            root_closed_awaiting_absence: None,
            slots,
            reason: "injected staging root disposition failure",
        });
    }
    if root_disposition_accepted {
        // 受理済みdispositionは再発行せず、期限内にchecked closeだけを行う。
    } else if unsafe {
        SetFileInformationByHandle(
            raw,
            FileDispositionInfo,
            &FILE_DISPOSITION_INFO {
                DeleteFile: ::windows::Win32::Foundation::BOOLEAN(1),
            } as *const _ as *const c_void,
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .is_err()
    {
        let retained = super::StagingRootLease {
            root_path: root_value.root_path,
            root_identity: root_value.root_identity,
            root_chain: root_value.root_chain,
            root: DirectoryLock(raw),
        };
        return Err(StagedTreeDeleteFailure {
            root: Some(retained),
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_disposition_accepted: false,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging root disposition was not accepted",
        });
    }
    root_disposition_accepted = true;
    if matches!(fault, Some(CleanupTestFault::ExpireAfterRootDisposition)) {
        wait_until_deadline(deadline);
    }
    if Instant::now() >= deadline {
        return Err(StagedTreeDeleteFailure {
            root: Some(super::StagingRootLease {
                root_path: root_value.root_path,
                root_identity: root_value.root_identity,
                root_chain: root_value.root_chain,
                root: DirectoryLock(raw),
            }),
            root_disposition_accepted,
            root_chain: Vec::new(),
            root_close_uncertain: None,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging cleanup deadline expired before root checked close",
        });
    }
    if matches!(
        fault,
        Some(CleanupTestFault::RootClose | CleanupTestFault::WorkerRootClose)
    ) {
        #[cfg(test)]
        if matches!(fault, Some(CleanupTestFault::WorkerRootClose)) {
            if let Some(capture) = uncertain_handle_capture {
                capture.store(raw.0 as isize, std::sync::atomic::Ordering::Release);
            } else {
                WORKER_CLOSE_UNCERTAIN_HANDLE
                    .store(raw.0 as isize, std::sync::atomic::Ordering::Release);
            }
        }
        return Err(StagedTreeDeleteFailure {
            root: None,
            root_chain: root_value.root_chain,
            root_close_uncertain: Some((raw, root_identity)),
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "injected staging root close uncertainty",
        });
    }
    if unsafe { CloseHandle(raw) }.is_err() {
        return Err(StagedTreeDeleteFailure {
            root: None,
            root_chain: root_value.root_chain,
            root_close_uncertain: Some((raw, root_identity)),
            root_disposition_accepted,
            root_closed_awaiting_absence: None,
            slots,
            reason: "staging root close is uncertain",
        });
    }
    let Some(root_file_index) = parse_identity(&root_identity) else {
        return Err(StagedTreeDeleteFailure {
            root: None,
            root_chain: root_value.root_chain,
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: Some(root_identity),
            slots,
            reason: "staging root identity is malformed after checked close",
        });
    };
    match parent_absence(
        &parent,
        expected_parent,
        &JournalSevenZipExtractIdentity {
            volume: expected_parent.volume,
            file_index: root_file_index,
        },
        deadline,
    ) {
        _ if matches!(fault, Some(CleanupTestFault::RootAbsence)) => {
            Err(StagedTreeDeleteFailure {
                root: None,
                root_chain: root_value.root_chain,
                root_close_uncertain: None,
                root_disposition_accepted,
                root_closed_awaiting_absence: Some(root_identity),
                slots,
                reason: "injected staging root absence query failure",
            })
        }
        Ok(true) => Ok(()),
        _ => Err(StagedTreeDeleteFailure {
            root: None,
            root_chain: root_value.root_chain,
            root_close_uncertain: None,
            root_disposition_accepted,
            root_closed_awaiting_absence: Some(root_identity),
            slots,
            reason: "staging root absence is unconfirmed",
        }),
    }
}

fn slot_components(slot: &StagedTreeDeleteSlot) -> Option<&[Vec<u16>]> {
    match slot {
        StagedTreeDeleteSlot::Held { node, .. } => Some(&node.relative_components),
        StagedTreeDeleteSlot::ClosedAwaitingAbsence {
            relative_components,
            ..
        }
        | StagedTreeDeleteSlot::CloseUncertain {
            relative_components,
            ..
        }
        | StagedTreeDeleteSlot::Removed {
            relative_components,
            ..
        } => Some(relative_components),
    }
}

fn delete_staged_slot(
    slots: &mut [StagedTreeDeleteSlot],
    index: usize,
    root: &super::StagingRootLease,
    deadline: Instant,
    fault: &mut Option<CleanupTestFault>,
    _uncertain_handle_capture: Option<&std::sync::atomic::AtomicIsize>,
) -> Result<(), &'static str> {
    #[cfg(test)]
    let uncertain_handle_capture = _uncertain_handle_capture;
    if Instant::now() >= deadline {
        return Err("staging cleanup deadline expired");
    }
    let relative = slot_components(&slots[index])
        .ok_or("staged child path is missing")?
        .to_vec();
    let parent_components = &relative[..relative.len().saturating_sub(1)];
    let parent_raw = if parent_components.is_empty() {
        root.root.0
    } else {
        let found = slots
            .iter()
            .find_map(|slot| match slot {
                StagedTreeDeleteSlot::Held { node, .. }
                    if node.relative_components == parent_components
                        && matches!(node.handle, super::PinnedSourceHandle::Directory(_)) =>
                {
                    match &node.handle {
                        super::PinnedSourceHandle::Directory(directory) => Some(directory),
                        _ => None,
                    }
                }
                _ => None,
            })
            .ok_or("staged parent handle is unavailable")?;
        found.0
    };
    // Borrow this already-held parent handle without manufacturing a second owner.
    let parent = ManuallyDrop::new(DirectoryLock(parent_raw));
    let parent_identity = super::directory_identity_text(&parent)
        .map_err(|_| "staged parent identity query failed")?;
    if let StagedTreeDeleteSlot::Held {
        node,
        disposition_accepted,
    } = &mut slots[index]
    {
        if matches!(
            (&node.node_kind, &node.handle),
            (
                crate::operations::journal::JournalDirectoryNodeKind::Directory,
                super::PinnedSourceHandle::File(_)
            ) | (
                crate::operations::journal::JournalDirectoryNodeKind::File,
                super::PinnedSourceHandle::Directory(_)
            )
        ) {
            return Err("staged child kind does not match retained handle");
        }
        if matches!(fault, Some(CleanupTestFault::ChildIdentity))
            || matches!(fault, Some(CleanupTestFault::ChildIdentityAt(failed)) if *failed == index)
        {
            *fault = None;
            return Err("injected staged child identity failure");
        }
        let actual = match &node.handle {
            super::PinnedSourceHandle::Directory(directory) => {
                super::directory_identity_text(directory)
            }
            super::PinnedSourceHandle::File(file) => {
                let id = super::regular_file_identity(file)
                    .map_err(|_| "staged child identity query failed")?;
                Ok(format!(
                    "{}:{:08x}{:08x}",
                    id.volume, id.index_high, id.index_low
                ))
            }
        }
        .map_err(|_| "staged child identity query failed")?;
        if matches!(fault, Some(CleanupTestFault::ExpireAfterChildIdentity(failed)) if *failed == index)
        {
            *fault = None;
            wait_until_deadline(deadline);
        }
        if Instant::now() >= deadline {
            return Err("staging cleanup deadline expired after child identity query");
        }
        let identity_was_observed = node.identity.is_some();
        if let Some(expected) = &node.identity {
            if actual != *expected {
                return Err("staged child identity changed");
            }
        } else {
            // 未観測identityは同じ保持handleから一度だけ採取し、失敗時もslotに残す。
            node.identity = Some(actual.clone());
        }
        if !identity_was_observed {
            return Err("staged child identity must be recorded before cleanup");
        }
        if let super::PinnedSourceHandle::Directory(directory) = &node.handle {
            if matches!(fault, Some(CleanupTestFault::DirectoryEmpty))
                || matches!(fault, Some(CleanupTestFault::DirectoryEmptyAt(failed)) if *failed == index)
            {
                *fault = None;
                return Err("injected staged directory empty-check failure");
            }
            if !enumerate_empty(directory) {
                return Err("staged directory is not empty");
            }
            if matches!(fault, Some(CleanupTestFault::ExpireAfterChildEmpty(failed)) if *failed == index)
            {
                *fault = None;
                wait_until_deadline(deadline);
            }
            if Instant::now() >= deadline {
                return Err("staging cleanup deadline expired after child empty query");
            }
        }
        if !*disposition_accepted {
            if matches!(fault, Some(CleanupTestFault::ChildDisposition))
                || matches!(fault, Some(CleanupTestFault::ChildDispositionAt(failed)) if *failed == index)
            {
                *fault = None;
                return Err("injected staged child disposition failure");
            }
            let raw = match &node.handle {
                super::PinnedSourceHandle::Directory(directory) => directory.0,
                super::PinnedSourceHandle::File(file) => super::raw(file),
            };
            let disposition = FILE_DISPOSITION_INFO {
                DeleteFile: ::windows::Win32::Foundation::BOOLEAN(1),
            };
            unsafe {
                SetFileInformationByHandle(
                    raw,
                    FileDispositionInfo,
                    &disposition as *const _ as *const c_void,
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            }
            .map_err(|_| "staged child disposition was not accepted")?;
            *disposition_accepted = true;
            if matches!(fault, Some(CleanupTestFault::ExpireAfterChildDisposition(failed)) if *failed == index)
            {
                *fault = None;
                wait_until_deadline(deadline);
            }
            if Instant::now() >= deadline {
                return Err("staging cleanup deadline expired before child checked close");
            }
        }
        let identity = node
            .identity
            .clone()
            .expect("identity observed from retained child handle");
        let components = node.relative_components.clone();
        let owned = std::mem::replace(
            &mut slots[index],
            StagedTreeDeleteSlot::Removed {
                relative_components: components.clone(),
                identity: identity.clone(),
            },
        );
        let StagedTreeDeleteSlot::Held { node, .. } = owned else {
            unreachable!()
        };
        let raw = match node.handle {
            super::PinnedSourceHandle::File(file) => file.into_raw_handle(),
            super::PinnedSourceHandle::Directory(directory) => {
                let directory = ManuallyDrop::new(directory);
                directory.0.0 as *mut c_void
            }
        };
        let handle = HANDLE(raw.cast());
        if matches!(fault, Some(CleanupTestFault::ChildClose)) {
            *fault = None;
            #[cfg(test)]
            if let Some(capture) = uncertain_handle_capture {
                capture.store(handle.0 as isize, std::sync::atomic::Ordering::Release);
            }
            slots[index] = StagedTreeDeleteSlot::CloseUncertain {
                relative_components: components,
                raw: handle,
                identity,
            };
            return Err("injected staged child close uncertainty");
        }
        if unsafe { CloseHandle(handle) }.is_err() {
            slots[index] = StagedTreeDeleteSlot::CloseUncertain {
                relative_components: components,
                raw: handle,
                identity,
            };
            return Err("staged child close is uncertain");
        }
        slots[index] = StagedTreeDeleteSlot::ClosedAwaitingAbsence {
            relative_components: components,
            identity,
        };
    }
    let identity = match &slots[index] {
        StagedTreeDeleteSlot::ClosedAwaitingAbsence { identity, .. } => identity.clone(),
        StagedTreeDeleteSlot::Removed { .. } => return Ok(()),
        StagedTreeDeleteSlot::CloseUncertain { .. } => {
            return Err("staged child close is uncertain");
        }
        StagedTreeDeleteSlot::Held { .. } => return Err("staged child disposition failed"),
    };
    let old = parse_identity(&identity).ok_or("staged child identity is malformed")?;
    let volume = parent_identity
        .split_once(':')
        .and_then(|(volume, _)| volume.parse::<u32>().ok())
        .ok_or("staged parent identity is malformed")?;
    let old_id = JournalSevenZipExtractIdentity {
        volume,
        file_index: old,
    };
    let parent_id = JournalSevenZipExtractIdentity {
        volume,
        file_index: parse_identity(&parent_identity)
            .ok_or("staged parent identity is malformed")?,
    };
    match parent_absence(&parent, &parent_id, &old_id, deadline) {
        Ok(true) => {
            slots[index] = StagedTreeDeleteSlot::Removed {
                relative_components: relative,
                identity,
            };
            Ok(())
        }
        _ => Err("staged child absence is unconfirmed"),
    }
}

fn parse_identity(identity: &str) -> Option<u64> {
    let (_, index) = identity.split_once(':')?;
    u64::from_str_radix(index, 16).ok()
}

fn enumerate_empty(directory: &DirectoryLock) -> bool {
    super::enumerate_directory_handle(directory, 1, 2).is_ok_and(|entries| entries.is_empty())
}

fn raw_root(root: DirectoryLock) -> HANDLE {
    let root = ManuallyDrop::new(root);
    root.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn parent_identity(parent: &DirectoryLock) -> JournalSevenZipExtractIdentity {
        let text = super::super::directory_identity_text(parent).unwrap();
        let (volume, index) = text.split_once(':').unwrap();
        JournalSevenZipExtractIdentity {
            volume: volume.parse().unwrap(),
            file_index: u64::from_str_radix(index, 16).unwrap(),
        }
    }

    fn empty_stage(
        label: &str,
    ) -> (
        tempfile::TempDir,
        super::VerifiedStagedTree,
        JournalSevenZipExtractIdentity,
    ) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let temp = tempfile::Builder::new()
            .prefix(&format!(
                "gpui-filer-extract-cleanup-{label}-{}-",
                std::process::id()
            ))
            .tempdir()
            .unwrap();
        let parent = super::super::open_staging_directory(temp.path()).unwrap();
        let expected_parent = parent_identity(&parent);
        let path = temp
            .path()
            .join(format!("stage-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let root = match super::super::retained_tree::create_root(&path) {
            super::super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("stage root creation failed: {other:?}"),
        };
        let stage = super::VerifiedStagedTree {
            root_path: root.root_path,
            root_identity: root.root_identity,
            root_chain: root.root_chain,
            root: root.root,
            nodes: Vec::new(),
            handles: Vec::new(),
        };
        (temp, stage, expected_parent)
    }

    fn stage_with_child(
        label: &str,
        directory: bool,
    ) -> (
        tempfile::TempDir,
        super::VerifiedStagedTree,
        JournalSevenZipExtractIdentity,
    ) {
        let (temp, mut stage, expected_parent) = empty_stage(label);
        let name = "child";
        let path = stage.root_path.join(name);
        if directory {
            std::fs::create_dir(&path).unwrap();
            let handle = super::super::open_pinned_directory(&path, true).unwrap();
            let identity = super::super::directory_identity_text(&handle).unwrap();
            stage.handles.push(super::VerifiedStagedNode {
                relative_components: vec![name.encode_utf16().collect()],
                node_kind: crate::operations::journal::JournalDirectoryNodeKind::Directory,
                identity,
                content_fact: String::new(),
                handle: super::PinnedSourceHandle::Directory(handle),
            });
        } else {
            std::fs::write(&path, b"staged").unwrap();
            use std::os::windows::fs::OpenOptionsExt;
            let handle = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
                .open(&path)
                .unwrap();
            let id = super::super::regular_file_identity(&handle).unwrap();
            stage.handles.push(super::VerifiedStagedNode {
                relative_components: vec![name.encode_utf16().collect()],
                node_kind: crate::operations::journal::JournalDirectoryNodeKind::File,
                identity: format!("{}:{:08x}{:08x}", id.volume, id.index_high, id.index_low),
                content_fact: String::new(),
                handle: super::PinnedSourceHandle::File(handle),
            });
        }
        (temp, stage, expected_parent)
    }

    fn stage_with_files(
        label: &str,
        names: &[&str],
    ) -> (
        tempfile::TempDir,
        super::VerifiedStagedTree,
        JournalSevenZipExtractIdentity,
    ) {
        let (temp, mut stage, expected_parent) = empty_stage(label);
        use std::os::windows::fs::OpenOptionsExt;
        for name in names {
            let path = stage.root_path.join(name);
            std::fs::write(&path, b"staged").unwrap();
            let handle = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
                .open(&path)
                .unwrap();
            let id = super::super::regular_file_identity(&handle).unwrap();
            stage.handles.push(super::VerifiedStagedNode {
                relative_components: vec![name.encode_utf16().collect()],
                node_kind: crate::operations::journal::JournalDirectoryNodeKind::File,
                identity: format!("{}:{:08x}{:08x}", id.volume, id.index_high, id.index_low),
                content_fact: String::new(),
                handle: super::PinnedSourceHandle::File(handle),
            });
        }
        (temp, stage, expected_parent)
    }

    fn stage_with_directories(
        label: &str,
        names: &[&str],
    ) -> (
        tempfile::TempDir,
        super::VerifiedStagedTree,
        JournalSevenZipExtractIdentity,
    ) {
        let (temp, mut stage, expected_parent) = empty_stage(label);
        for name in names {
            let path = stage.root_path.join(name);
            std::fs::create_dir(&path).unwrap();
            let handle = super::super::open_pinned_directory(&path, true).unwrap();
            let identity = super::super::directory_identity_text(&handle).unwrap();
            stage.handles.push(super::VerifiedStagedNode {
                relative_components: vec![name.encode_utf16().collect()],
                node_kind: crate::operations::journal::JournalDirectoryNodeKind::Directory,
                identity,
                content_fact: String::new(),
                handle: super::PinnedSourceHandle::Directory(handle),
            });
        }
        (temp, stage, expected_parent)
    }

    fn delete_with_fault(
        staged: super::VerifiedStagedTree,
        expected_parent: &JournalSevenZipExtractIdentity,
        parent_path: &Path,
        fault: CleanupTestFault,
    ) -> Result<(), StagedTreeDeleteFailure> {
        let super::VerifiedStagedTree {
            root_path,
            root_identity,
            root_chain,
            root,
            handles,
            ..
        } = staged;
        let mut slots = handles
            .into_iter()
            .map(|node| StagedTreeDeleteSlot::Held {
                node: CleanupNode {
                    relative_components: node.relative_components,
                    node_kind: node.node_kind,
                    identity: Some(node.identity),
                    handle: node.handle,
                },
                disposition_accepted: false,
            })
            .collect::<Vec<_>>();
        slots.sort_by_key(|slot| {
            std::cmp::Reverse(slot_components(slot).map_or(0, |parts| parts.len()))
        });
        delete_staged_tree_custody(
            Some(super::StagingRootLease {
                root_path,
                root_identity,
                root_chain,
                root,
            }),
            slots,
            expected_parent,
        parent_path,
        Instant::now()
                + if matches!(
                    fault,
                    CleanupTestFault::ExpireAfterRootIdentity
                        | CleanupTestFault::ExpireAfterRootEmpty
                        | CleanupTestFault::ExpireAfterRootDisposition
                        | CleanupTestFault::ExpireAfterChildIdentity(_)
                        | CleanupTestFault::ExpireAfterChildEmpty(_)
                        | CleanupTestFault::ExpireAfterChildDisposition(_)
                ) {
                    Duration::from_millis(30)
                } else {
                    CLEANUP_BUDGET
                },
            Some(fault),
            None,
            false,
        )
    }

    fn retry_tree_delete_with_fault(
        failure: StagedTreeDeleteFailure,
        expected_parent: &JournalSevenZipExtractIdentity,
        parent_path: &Path,
        deadline: Instant,
        fault: Option<CleanupTestFault>,
    ) -> Result<(), StagedTreeDeleteFailure> {
        delete_staged_tree_custody(
            failure.root,
            failure.slots,
            expected_parent,
            parent_path,
            deadline,
            fault,
            None,
            failure.root_disposition_accepted,
        )
    }

    #[test]
    fn exact_empty_stage_is_disposition_closed_and_removed_by_identity() {
        let (temp, stage, expected_parent) = empty_stage("empty");
        let path = stage.root_path.clone();
        delete_staged_tree(
            stage,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn foreign_root_child_retains_stage_without_recursive_delete() {
        let (temp, stage, expected_parent) = empty_stage("foreign");
        let path = stage.root_path.clone();
        let sentinel = path.join("foreign.txt");
        std::fs::write(&sentinel, b"foreign").unwrap();
        let failure = delete_staged_tree(
            stage,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap_err();
        assert!(failure.root.is_some());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"foreign");
        drop(failure);
        std::fs::remove_file(sentinel).unwrap();
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn foreign_junction_blocks_root_cleanup_without_touching_its_target() {
        let (temp, stage, expected_parent) = empty_stage("foreign-junction");
        let path = stage.root_path.clone();
        let target = temp.path().join("junction-target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("sentinel"), b"foreign target").unwrap();
        crate::operations::privileged::create_junction_direct(&target, &path.join("junction"))
            .expect("create native test junction");
        let failure = delete_staged_tree(
            stage,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap_err();
        assert!(failure.root.is_some());
        assert_eq!(
            std::fs::read(target.join("sentinel")).unwrap(),
            b"foreign target"
        );
        drop(failure);
        std::fs::remove_dir(path.join("junction")).unwrap();
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn retained_empty_stage_resumes_after_foreign_child_is_removed() {
        let (temp, stage, expected_parent) = empty_stage("resume-root");
        let path = stage.root_path.clone();
        let sentinel = path.join("foreign.txt");
        std::fs::write(&sentinel, b"foreign").unwrap();
        let failure = delete_staged_tree(
            stage,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap_err();
        assert_eq!(failure.reason, "staging root contains an unowned child");
        assert!(failure.root.is_some());
        std::fs::remove_file(sentinel).unwrap();
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn empty_created_root_cleanup_requires_identity_observed_from_retained_handle() {
        let (temp, stage, expected_parent) = empty_stage("retained-root");
        let path = stage.root_path.clone();
        let expected_identity = stage.root_identity.clone();
        let super::VerifiedStagedTree {
            root_path,
            root_chain,
            root,
            ..
        } = stage;
        delete_retained_root(
            super::retained_tree::RetainedRoot {
                root_path,
                root_identity: None,
                root_chain,
                root,
            },
            &expected_identity,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn expired_cleanup_deadline_retains_empty_stage_without_disposition() {
        let (temp, stage, expected_parent) = empty_stage("expired");
        let path = stage.root_path.clone();
        let failure = delete_staged_tree(
            stage,
            &expected_parent,
            temp.path(),
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(failure.root.is_some());
        assert!(path.exists());
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn retained_child_identity_failure_resumes_without_losing_slot() {
        let (temp, stage, expected_parent) = stage_with_child("identity-fault", false);
        let root = stage.root_path.clone();
        let child = root.join("child");
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::ChildIdentity,
        )
        .unwrap_err();
        assert_eq!(failure.slots.len(), 1);
        assert!(matches!(
            failure.slots[0],
            StagedTreeDeleteSlot::Held { .. }
        ));
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!child.exists() && !root.exists());
    }

    #[test]
    fn middle_child_failure_retains_completed_current_and_unprocessed_slots() {
        let (temp, stage, expected_parent) = stage_with_files("middle-slot", &["a", "b", "c"]);
        let root = stage.root_path.clone();
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::ChildIdentityAt(1),
        )
        .unwrap_err();
        assert!(matches!(
            failure.slots[0],
            StagedTreeDeleteSlot::Removed { .. }
        ));
        assert!(matches!(
            failure.slots[1],
            StagedTreeDeleteSlot::Held { .. }
        ));
        assert!(matches!(
            failure.slots[2],
            StagedTreeDeleteSlot::Held { .. }
        ));
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn middle_identity_empty_and_disposition_faults_keep_completed_current_and_remaining_slots() {
        for (label, fault) in [
            ("middle-id", CleanupTestFault::ChildIdentityAt(1)),
            ("middle-empty", CleanupTestFault::DirectoryEmptyAt(1)),
            (
                "middle-disposition",
                CleanupTestFault::ChildDispositionAt(1),
            ),
        ] {
            let (temp, stage, expected_parent) = stage_with_directories(label, &["a", "b", "c"]);
            let root = stage.root_path.clone();
            let failure =
                delete_with_fault(stage, &expected_parent, temp.path(), fault).unwrap_err();
            assert!(matches!(
                failure.slots[0],
                StagedTreeDeleteSlot::Removed { .. }
            ));
            assert!(matches!(
                failure.slots[1],
                StagedTreeDeleteSlot::Held { .. }
            ));
            assert!(matches!(
                failure.slots[2],
                StagedTreeDeleteSlot::Held { .. }
            ));
            resume_staged_tree_delete(
                failure,
                &expected_parent,
                temp.path(),
                Instant::now() + CLEANUP_BUDGET,
            )
            .unwrap();
            assert!(!root.exists());
        }
    }

    #[test]
    fn retained_child_disposition_failure_resumes_without_reissuing_accepted_work() {
        let (temp, stage, expected_parent) = stage_with_child("disposition-fault", false);
        let root = stage.root_path.clone();
        let child = root.join("child");
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::ChildDisposition,
        )
        .unwrap_err();
        assert!(matches!(
            failure.slots[0],
            StagedTreeDeleteSlot::Held {
                disposition_accepted: false,
                ..
            }
        ));
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!child.exists() && !root.exists());
    }

    #[test]
    fn retained_directory_empty_query_failure_resumes_with_directory_slot() {
        let (temp, stage, expected_parent) = stage_with_child("empty-fault", true);
        let root = stage.root_path.clone();
        let child = root.join("child");
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::DirectoryEmpty,
        )
        .unwrap_err();
        assert!(matches!(
            failure.slots[0],
            StagedTreeDeleteSlot::Held { .. }
        ));
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!child.exists() && !root.exists());
    }

    #[test]
    fn unknown_child_identity_is_observed_on_same_retained_handle_and_kept_on_failure() {
        let (temp, stage, expected_parent) = stage_with_child("unknown-identity", false);
        let path = stage.root_path.clone();
        let super::VerifiedStagedTree {
            root_path,
            root_identity,
            root_chain,
            root,
            mut handles,
            ..
        } = stage;
        let node = handles.pop().unwrap();
        let retained = super::spool_tree::retained_tree_for_cleanup_test(
            super::StagingRootLease {
                root_path,
                root_identity,
                root_chain,
                root,
            },
            "child",
            node.handle,
            node.node_kind,
            None,
        );
        let failure = delete_retained_spool_tree(
            retained,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap_err();
        assert!(matches!(
            failure.slots[0],
            StagedTreeDeleteSlot::Held {
                node: CleanupNode {
                    identity: Some(_),
                    ..
                },
                ..
            }
        ));
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!path.exists());
    }

    fn directory_record(file_id: u64, name: &str, next: u32) -> Vec<u8> {
        let wide = name
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let fixed = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let mut bytes = vec![0; fixed + wide.len()];
        bytes[offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset)..][..4]
            .copy_from_slice(&next.to_le_bytes());
        bytes[offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength)..][..4]
            .copy_from_slice(&(wide.len() as u32).to_le_bytes());
        let id = offset_of!(FILE_ID_BOTH_DIR_INFO, FileId);
        bytes[id..id + 8].copy_from_slice(&file_id.to_le_bytes());
        bytes[fixed..].copy_from_slice(&wide);
        bytes
    }

    #[test]
    fn directory_page_parser_accepts_complete_records_and_rejects_bad_offsets() {
        let first = directory_record(7, "one", 0);
        assert_eq!(
            parse_directory_page(&first).unwrap(),
            vec![(7, "one".encode_utf16().collect())]
        );

        let mut unaligned = directory_record(7, "one", 0);
        unaligned[offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset)..][..4]
            .copy_from_slice(&5u32.to_le_bytes());
        assert!(parse_directory_page(&unaligned).is_err());

        let mut overlaps_name = directory_record(7, "one", 0);
        let too_short = (offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) + 2) as u32;
        overlaps_name[offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset)..][..4]
            .copy_from_slice(&too_short.to_le_bytes());
        assert!(parse_directory_page(&overlaps_name).is_err());

        let truncated = &first[..offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) - 1];
        assert!(parse_directory_page(truncated).is_err());
    }

    #[test]
    fn uncertain_child_close_is_quarantined_and_never_retried() {
        let (temp, stage, expected_parent) = stage_with_child("close-uncertain", false);
        let root = stage.root_path.clone();
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::ChildClose,
        )
        .unwrap_err();
        assert!(matches!(
            failure.slots[0],
            StagedTreeDeleteSlot::CloseUncertain { .. }
        ));
        let failure = resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap_err();
        let slot = failure.slots.into_iter().next().unwrap();
        let StagedTreeDeleteSlot::CloseUncertain { raw, .. } = slot else {
            panic!("raw handle remains quarantined")
        };
        let mut flags = 0u32;
        assert!(
            unsafe { ::windows::Win32::Foundation::GetHandleInformation(raw, &mut flags) }.is_ok()
        );
        assert!(unsafe { CloseHandle(raw) }.is_ok());
        drop(failure.root);
        drop(failure.root_chain);
        assert!(!root.join("child").exists());
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn identity_scan_finds_old_id_after_multiple_native_pages() {
        let temp = tempfile::tempdir().unwrap();
        let parent = super::super::open_staging_directory(temp.path()).unwrap();
        let expected_parent = parent_identity(&parent);
        let mut old_ids = Vec::new();
        for index in 0..1200 {
            let path = temp.path().join(format!("page-entry-{index:04}"));
            std::fs::write(&path, b"x").unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let identity = super::super::regular_file_identity(&file).unwrap();
            old_ids.push(JournalSevenZipExtractIdentity {
                volume: identity.volume,
                file_index: (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
            });
        }
        let mut page = vec![0u64; 8 * 1024];
        let page_bytes = u32::try_from(page.len() * size_of::<u64>()).unwrap();
        unsafe {
            GetFileInformationByHandleEx(
                parent.0,
                FileIdBothDirectoryRestartInfo,
                page.as_mut_ptr().cast(),
                page_bytes,
            )
        }
        .unwrap();
        let first_page_ids = parse_directory_page(unsafe {
            std::slice::from_raw_parts(page.as_ptr().cast(), page.len() * size_of::<u64>())
        })
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect::<std::collections::HashSet<_>>();
        let old_id = old_ids
            .into_iter()
            .find(|identity| !first_page_ids.contains(&identity.file_index))
            .expect("a created file ID is absent from the actual first native page");
        let (absent, pages_seen) = parent_absence_with_pages(
            &parent,
            &expected_parent,
            &old_id,
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!absent);
        assert!(pages_seen > 1, "scan explicitly continued beyond page one");
    }

    #[test]
    fn root_identity_empty_and_disposition_failures_keep_root_and_resume() {
        for (label, fault) in [
            ("root-id", CleanupTestFault::RootIdentity),
            ("root-empty", CleanupTestFault::RootEmpty),
            ("root-disposition", CleanupTestFault::RootDisposition),
        ] {
            let (temp, stage, expected_parent) = empty_stage(label);
            let root = stage.root_path.clone();
            let failure =
                delete_with_fault(stage, &expected_parent, temp.path(), fault).unwrap_err();
            assert!(failure.root.is_some());
            resume_staged_tree_delete(
                failure,
                &expected_parent,
                temp.path(),
                Instant::now() + CLEANUP_BUDGET,
            )
            .unwrap();
            assert!(!root.exists());
        }
    }

    #[test]
    fn root_deadline_after_queries_retains_handle_and_resumes_accepted_disposition() {
        for (label, fault, accepted) in [
            (
                "root-deadline-identity",
                CleanupTestFault::ExpireAfterRootIdentity,
                false,
            ),
            (
                "root-deadline-empty",
                CleanupTestFault::ExpireAfterRootEmpty,
                false,
            ),
            (
                "root-deadline-close",
                CleanupTestFault::ExpireAfterRootDisposition,
                true,
            ),
        ] {
            let (temp, stage, expected_parent) = empty_stage(label);
            let root = stage.root_path.clone();
            let failure =
                delete_with_fault(stage, &expected_parent, temp.path(), fault).unwrap_err();
            assert!(failure.root.is_some());
            assert_eq!(failure.root_disposition_accepted, accepted);
            assert!(root.exists());
            resume_staged_tree_delete(
                failure,
                &expected_parent,
                temp.path(),
                Instant::now() + CLEANUP_BUDGET,
            )
            .unwrap();
            assert!(!root.exists());
        }
    }

    #[test]
    fn accepted_root_disposition_survives_expired_retry_and_completes() {
        let (temp, stage, expected_parent) = empty_stage("root-disposition-expired-retry");
        let root = stage.root_path.clone();
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::ExpireAfterRootDisposition,
        )
        .unwrap_err();
        assert!(failure.root_disposition_accepted);
        let expired = Instant::now() - Duration::from_millis(1);
        let failure =
            resume_staged_tree_delete(failure, &expected_parent, temp.path(), expired).unwrap_err();
        assert!(failure.root.is_some());
        assert!(failure.root_disposition_accepted);
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn accepted_root_disposition_survives_query_and_parent_retry_without_reissue() {
        for (label, fail_parent) in [("query", false), ("parent", true)] {
            let (temp, stage, expected_parent) =
                empty_stage(&format!("root-disposition-{label}-retry"));
            let root = stage.root_path.clone();
            let failure = delete_with_fault(
                stage,
                &expected_parent,
                temp.path(),
                CleanupTestFault::ExpireAfterRootDisposition,
            )
            .unwrap_err();
            assert!(failure.root_disposition_accepted);
            let mut wrong_parent = expected_parent.clone();
            wrong_parent.file_index = wrong_parent.file_index.wrapping_add(1);
            let failure = if fail_parent {
                retry_tree_delete_with_fault(
                    failure,
                    &wrong_parent,
                    temp.path(),
                    Instant::now() + CLEANUP_BUDGET,
                    None,
                )
            } else {
                retry_tree_delete_with_fault(
                    failure,
                    &expected_parent,
                    temp.path(),
                    Instant::now() + CLEANUP_BUDGET,
                    Some(CleanupTestFault::RootIdentity),
                )
            }
            .unwrap_err();
            assert!(failure.root.is_some());
            assert!(failure.root_disposition_accepted);
            let final_parent = if fail_parent {
                &expected_parent
            } else {
                &expected_parent
            };
            retry_tree_delete_with_fault(
                failure,
                final_parent,
                temp.path(),
                Instant::now() + CLEANUP_BUDGET,
                Some(CleanupTestFault::RootDisposition),
            )
            .unwrap();
            assert!(!root.exists());
        }
    }

    #[test]
    fn child_deadline_after_empty_and_disposition_queries_retains_and_resumes() {
        for (label, fault, accepted) in [
            (
                "child-deadline-identity",
                CleanupTestFault::ExpireAfterChildIdentity(0),
                false,
            ),
            (
                "child-deadline-empty",
                CleanupTestFault::ExpireAfterChildEmpty(0),
                false,
            ),
            (
                "child-deadline-close",
                CleanupTestFault::ExpireAfterChildDisposition(0),
                true,
            ),
        ] {
            let (temp, stage, expected_parent) = stage_with_child(label, true);
            let root = stage.root_path.clone();
            let failure =
                delete_with_fault(stage, &expected_parent, temp.path(), fault).unwrap_err();
            assert!(root.exists());
            assert!(matches!(
                &failure.slots[0],
                StagedTreeDeleteSlot::Held { disposition_accepted, .. }
                    if *disposition_accepted == accepted
            ));
            resume_staged_tree_delete(
                failure,
                &expected_parent,
                temp.path(),
                Instant::now() + CLEANUP_BUDGET,
            )
            .unwrap();
            assert!(!root.exists());
        }
    }

    #[test]
    fn root_close_uncertainty_is_quarantined_and_absence_retry_is_supported() {
        let (temp, stage, expected_parent) = empty_stage("root-close");
        let root = stage.root_path.clone();
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::RootClose,
        )
        .unwrap_err();
        let (raw, _) = failure
            .root_close_uncertain
            .expect("one uncertain root handle");
        assert!(unsafe { CloseHandle(raw) }.is_ok());
        drop(failure.root_chain);
        assert!(!root.exists());

        let (temp, stage, expected_parent) = empty_stage("root-absence");
        let root = stage.root_path.clone();
        let failure = delete_with_fault(
            stage,
            &expected_parent,
            temp.path(),
            CleanupTestFault::RootAbsence,
        )
        .unwrap_err();
        assert!(failure.root_closed_awaiting_absence.is_some());
        resume_staged_tree_delete(
            failure,
            &expected_parent,
            temp.path(),
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn delete_pending_alias_blocks_absence_until_last_handle_closes() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let parent = super::super::open_staging_directory(temp.path()).unwrap();
        let expected_parent = parent_identity(&parent);
        let path = temp.path().join("alias-held");
        let owner = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
            .open(&path)
            .unwrap();
        let alias = std::fs::File::open(&path).unwrap();
        let info = super::super::regular_file_identity(&owner).unwrap();
        let identity = JournalSevenZipExtractIdentity {
            volume: info.volume,
            file_index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        };
        unsafe {
            SetFileInformationByHandle(
                super::raw(&owner),
                FileDispositionInfo,
                &FILE_DISPOSITION_INFO {
                    DeleteFile: ::windows::Win32::Foundation::BOOLEAN(1),
                } as *const _ as *const c_void,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .unwrap();
        }
        drop(owner);
        assert!(
            !parent_absence(
                &parent,
                &expected_parent,
                &identity,
                Instant::now() + CLEANUP_BUDGET
            )
            .unwrap()
        );
        drop(alias);
        assert!(
            parent_absence(
                &parent,
                &expected_parent,
                &identity,
                Instant::now() + CLEANUP_BUDGET
            )
            .unwrap()
        );
        std::fs::write(&path, b"replacement with a new file ID").unwrap();
        assert!(
            parent_absence(
                &parent,
                &expected_parent,
                &identity,
                Instant::now() + CLEANUP_BUDGET
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"replacement with a new file ID"
        );
    }

    #[test]
    fn spool_identity_parent_and_disposition_failures_retain_the_raw_file() {
        for fault in [
            CleanupTestFault::SpoolIdentity,
            CleanupTestFault::SpoolParent,
            CleanupTestFault::SpoolDisposition,
        ] {
            use std::os::windows::fs::OpenOptionsExt;
            let temp = tempfile::tempdir().unwrap();
            let parent = super::super::open_staging_directory(temp.path()).unwrap();
            let expected_parent = parent_identity(&parent);
            let path = temp.path().join("spool");
            let file = std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
                .open(&path)
                .unwrap();
            let info = super::super::regular_file_identity(&file).unwrap();
            let identity = JournalSevenZipExtractIdentity {
                volume: info.volume,
                file_index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
            };
            let failure = delete_spool_inner(
                file,
                &identity,
                temp.path(),
                &expected_parent,
                Instant::now() + CLEANUP_BUDGET,
                false,
                Some(fault),
            )
            .unwrap_err();
            let SpoolDeleteFailure::Retained { .. } = failure else {
                panic!("pre-close fault retains file")
            };
            assert!(path.is_file());
            resume_spool_delete(
                failure,
                &identity,
                temp.path(),
                &expected_parent,
                Instant::now() + CLEANUP_BUDGET,
            )
            .unwrap();
            assert!(!path.exists());
        }
    }

    #[test]
    fn expired_spool_cleanup_retains_file_before_disposition_and_resumes() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let parent = super::super::open_staging_directory(temp.path()).unwrap();
        let expected_parent = parent_identity(&parent);
        let path = temp.path().join("spool-expired");
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
            .open(&path)
            .unwrap();
        let info = super::super::regular_file_identity(&file).unwrap();
        let identity = JournalSevenZipExtractIdentity {
            volume: info.volume,
            file_index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        };
        let failure = delete_spool(
            file,
            &identity,
            temp.path(),
            &expected_parent,
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(matches!(
            &failure,
            SpoolDeleteFailure::Retained {
                disposition_accepted: false,
                ..
            }
        ));
        assert!(path.exists());
        resume_spool_delete(
            failure,
            &identity,
            temp.path(),
            &expected_parent,
            Instant::now() + CLEANUP_BUDGET,
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn spool_close_uncertainty_is_quarantined_and_absence_can_be_rechecked() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let parent = super::super::open_staging_directory(temp.path()).unwrap();
        let expected_parent = parent_identity(&parent);
        let path = temp.path().join("spool");
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
            .open(&path)
            .unwrap();
        let info = super::super::regular_file_identity(&file).unwrap();
        let identity = JournalSevenZipExtractIdentity {
            volume: info.volume,
            file_index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        };
        let failure = delete_spool_inner(
            file,
            &identity,
            temp.path(),
            &expected_parent,
            Instant::now() + CLEANUP_BUDGET,
            false,
            Some(CleanupTestFault::SpoolClose),
        )
        .unwrap_err();
        let SpoolDeleteFailure::CloseUncertain { raw, .. } = failure else {
            panic!("close uncertainty quarantines raw handle")
        };
        assert!(path.exists());
        assert!(unsafe { CloseHandle(raw) }.is_ok());
        assert!(
            confirm_spool_absence(
                &identity,
                temp.path(),
                &expected_parent,
                Instant::now() + CLEANUP_BUDGET
            )
            .unwrap()
        );

        let path = temp.path().join("spool-absence");
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .access_mode(0x0001_0000 | 0x8000_0000 | 0x4000_0000)
            .open(&path)
            .unwrap();
        let info = super::super::regular_file_identity(&file).unwrap();
        let identity = JournalSevenZipExtractIdentity {
            volume: info.volume,
            file_index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        };
        let failure = delete_spool_inner(
            file,
            &identity,
            temp.path(),
            &expected_parent,
            Instant::now() + CLEANUP_BUDGET,
            false,
            Some(CleanupTestFault::SpoolAbsence),
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            SpoolDeleteFailure::AwaitingAbsence { .. }
        ));
        assert!(
            confirm_spool_absence(
                &identity,
                temp.path(),
                &expected_parent,
                Instant::now() + CLEANUP_BUDGET
            )
            .unwrap()
        );
        assert!(!path.exists());
    }
}

fn parent_matches(directory: &DirectoryLock, expected: &JournalSevenZipExtractIdentity) -> bool {
    super::directory_identity(directory).is_ok_and(|identity| {
        identity.volume == expected.volume
            && ((u64::from(identity.index_high) << 32) | u64::from(identity.index_low))
                == expected.file_index
    })
}

fn parent_absence(
    directory: &DirectoryLock,
    expected_parent: &JournalSevenZipExtractIdentity,
    old: &JournalSevenZipExtractIdentity,
    deadline: Instant,
) -> Result<bool, ()> {
    parent_absence_with_pages(directory, expected_parent, old, deadline).map(|(absent, _)| absent)
}

fn parent_absence_with_pages(
    directory: &DirectoryLock,
    expected_parent: &JournalSevenZipExtractIdentity,
    old: &JournalSevenZipExtractIdentity,
    deadline: Instant,
) -> Result<(bool, usize), ()> {
    if !parent_matches(directory, expected_parent) || old.volume != expected_parent.volume {
        return Err(());
    }
    let mut entries_seen = 0usize;
    let mut name_bytes = 0usize;
    let mut pages_seen = 0usize;
    let mut buffer = vec![0u64; 8 * 1024];
    let bytes = u32::try_from(buffer.len() * size_of::<u64>()).map_err(|_| ())?;
    let mut class: FILE_INFO_BY_HANDLE_CLASS = FileIdBothDirectoryRestartInfo;
    loop {
        if Instant::now() >= deadline {
            return Err(());
        }
        let result = unsafe {
            GetFileInformationByHandleEx(directory.0, class, buffer.as_mut_ptr().cast(), bytes)
        };
        if let Err(error) = result {
            if error.code() == ::windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                break;
            }
            return Err(());
        }
        class = FileIdBothDirectoryInfo;
        pages_seen = pages_seen.checked_add(1).ok_or(())?;
        let raw = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().cast::<u8>(),
                buffer.len() * size_of::<u64>(),
            )
        };
        for (file_id, name) in parse_directory_page(raw)? {
            if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                entries_seen = entries_seen.checked_add(1).ok_or(())?;
                name_bytes = name_bytes
                    .checked_add(name.len() * size_of::<u16>())
                    .ok_or(())?;
                if entries_seen > SCAN_ENTRY_LIMIT
                    || name_bytes > SCAN_NAME_BYTES_LIMIT
                    || Instant::now() >= deadline
                {
                    return Err(());
                }
                if file_id == old.file_index {
                    return Ok((false, pages_seen));
                }
            }
        }
    }
    if Instant::now().ge(&deadline) || !parent_matches(directory, expected_parent) {
        return Err(());
    }
    Ok((true, pages_seen))
}

fn parse_directory_page(bytes: &[u8]) -> Result<Vec<(u64, Vec<u16>)>, ()> {
    let fixed = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let next_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset);
    let name_length_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
    let file_id_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileId);
    let mut parsed = Vec::new();
    let mut offset = 0usize;
    loop {
        let header_end = offset.checked_add(fixed).ok_or(())?;
        if header_end > bytes.len() {
            return Err(());
        }
        let next = read_u32(bytes, offset.checked_add(next_offset).ok_or(())?)? as usize;
        let name_bytes =
            read_u32(bytes, offset.checked_add(name_length_offset).ok_or(())?)? as usize;
        if name_bytes == 0 || name_bytes % size_of::<u16>() != 0 {
            return Err(());
        }
        let name_start = header_end;
        let name_end = name_start.checked_add(name_bytes).ok_or(())?;
        if name_end > bytes.len() {
            return Err(());
        }
        let mut name = Vec::with_capacity(name_bytes / size_of::<u16>());
        for pair in bytes[name_start..name_end].chunks_exact(2) {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                return Err(());
            }
            name.push(unit);
        }
        let id_start = offset.checked_add(file_id_offset).ok_or(())?;
        let id_end = id_start.checked_add(size_of::<i64>()).ok_or(())?;
        if id_end > header_end {
            return Err(());
        }
        let file_id = u64::from_le_bytes(bytes[id_start..id_end].try_into().map_err(|_| ())?);
        parsed.push((file_id, name));
        if next == 0 {
            break;
        }
        if next % align_of::<u64>() != 0 || next < fixed.checked_add(name_bytes).ok_or(())? {
            return Err(());
        }
        offset = offset
            .checked_add(next)
            .filter(|next| *next < bytes.len())
            .ok_or(())?;
    }
    Ok(parsed)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ()> {
    let end = offset.checked_add(size_of::<u32>()).ok_or(())?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
