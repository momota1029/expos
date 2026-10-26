//! Application-level registration for live windows.

use crate::shell::FilerShell;
use crate::{persistence::SessionBarrierId, session_store::SessionSnapshot, shell::WorkspaceId};
use gpui::{Global, WindowHandle};
use std::collections::HashMap;

#[derive(Clone)]
pub(crate) struct WindowRegistration {
    pub(crate) id: u64,
    pub(crate) incarnation: u64,
    pub(crate) handle: WindowHandle<FilerShell>,
}

#[derive(Default)]
pub(crate) struct WindowCoordinator {
    windows: HashMap<u64, WindowRegistration>,
    next_incarnation: u64,
    active_window: Option<u64>,
    workspace_drag: Option<WorkspaceDrag>,
    workspace_transfer: Option<WorkspaceTransferIntent>,
    deferred_workspace_closes: Vec<(u64, u64)>,
}

#[derive(Clone)]
pub(crate) struct WorkspaceDrag {
    pub(crate) source_window: u64,
    pub(crate) source_incarnation: u64,
    pub(crate) source_workspace: WorkspaceId,
    pub(crate) source_order: Vec<WorkspaceId>,
    pub(crate) start_screen: (f32, f32),
    pub(crate) started: bool,
    pub(crate) target: Option<WorkspaceDropTarget>,
}

#[derive(Clone)]
pub(crate) struct WorkspaceDropTarget {
    pub(crate) window: u64,
    pub(crate) incarnation: u64,
    pub(crate) workspace: WorkspaceId,
    pub(crate) order: Vec<WorkspaceId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceTransferPhase {
    Candidate,
    Rollback,
    Committed,
}

#[derive(Clone)]
pub(crate) struct WorkspaceTransferIntent {
    pub(crate) source_window: u64,
    pub(crate) source_incarnation: u64,
    pub(crate) source_workspace: WorkspaceId,
    pub(crate) source_order: Vec<WorkspaceId>,
    pub(crate) destination_window: u64,
    pub(crate) destination_incarnation: u64,
    pub(crate) target_workspace: WorkspaceId,
    pub(crate) destination_order: Vec<WorkspaceId>,
    pub(crate) barrier_id: SessionBarrierId,
    pub(crate) phase: WorkspaceTransferPhase,
    pub(crate) candidate: SessionSnapshot,
    pub(crate) retry_at: std::time::Instant,
    pub(crate) cancelled: bool,
    pub(crate) close_after: Vec<(u64, u64)>,
}

impl Global for WindowCoordinator {}

impl WindowCoordinator {
    pub(crate) fn begin_workspace_drag(
        &mut self,
        source_window: u64,
        source_incarnation: u64,
        source_workspace: WorkspaceId,
        source_order: Vec<WorkspaceId>,
        start_screen: (f32, f32),
    ) -> bool {
        if self.workspace_drag.is_some()
            || self.workspace_transfer.is_some()
            || !self.contains(source_window, source_incarnation)
            || !source_order.contains(&source_workspace)
        {
            return false;
        }
        self.workspace_drag = Some(WorkspaceDrag {
            source_window,
            source_incarnation,
            source_workspace,
            source_order,
            start_screen,
            started: false,
            target: None,
        });
        true
    }

    pub(crate) fn workspace_drag(&self) -> Option<WorkspaceDrag> {
        self.workspace_drag.clone()
    }

    pub(crate) fn set_workspace_drop_target(
        &mut self,
        target: Option<WorkspaceDropTarget>,
    ) -> bool {
        if let Some(drag) = self.workspace_drag.as_mut() {
            if drag.target.as_ref().is_some_and(|old| {
                target.as_ref().is_some_and(|new| {
                    old.window == new.window
                        && old.incarnation == new.incarnation
                        && old.workspace == new.workspace
                        && old.order == new.order
                })
            }) || drag.target.is_none() && target.is_none()
            {
                return false;
            }
            drag.target = target;
            return true;
        }
        false
    }

    pub(crate) fn mark_workspace_drag_started(&mut self) {
        if let Some(drag) = self.workspace_drag.as_mut() {
            drag.started = true;
        }
    }

    pub(crate) fn workspace_transfer(&self) -> Option<&WorkspaceTransferIntent> {
        self.workspace_transfer.as_ref()
    }

    pub(crate) fn workspace_transfer_mut(&mut self) -> Option<&mut WorkspaceTransferIntent> {
        self.workspace_transfer.as_mut()
    }

    pub(crate) fn start_workspace_transfer(&mut self, transfer: WorkspaceTransferIntent) -> bool {
        if self.workspace_transfer.is_some()
            || !self.contains(transfer.source_window, transfer.source_incarnation)
            || !self.contains(
                transfer.destination_window,
                transfer.destination_incarnation,
            )
            || !self.workspace_drag.as_ref().is_some_and(|drag| {
                drag.source_window == transfer.source_window
                    && drag.source_incarnation == transfer.source_incarnation
                    && drag.source_workspace == transfer.source_workspace
                    && drag.source_order == transfer.source_order
                    && drag.target.as_ref().is_some_and(|target| {
                        target.window == transfer.destination_window
                            && target.incarnation == transfer.destination_incarnation
                            && target.workspace == transfer.target_workspace
                            && target.order == transfer.destination_order
                    })
            })
        {
            return false;
        }
        self.workspace_drag = None;
        self.workspace_transfer = Some(transfer);
        true
    }

    pub(crate) fn finish_workspace_transfer(&mut self) {
        if let Some(transfer) = self.workspace_transfer.take() {
            self.defer_workspace_closes(&transfer.close_after);
        }
    }

    pub(crate) fn defer_workspace_closes(&mut self, identities: &[(u64, u64)]) {
        for identity in identities {
            if !self.deferred_workspace_closes.contains(identity) {
                self.deferred_workspace_closes.push(*identity);
            }
        }
    }

    pub(crate) fn next_deferred_workspace_close(&self) -> Option<(u64, u64)> {
        self.deferred_workspace_closes.first().copied()
    }

    pub(crate) fn resolve_deferred_workspace_close(&mut self, identity: (u64, u64)) {
        if let Some(index) = self
            .deferred_workspace_closes
            .iter()
            .position(|pending| *pending == identity)
        {
            self.deferred_workspace_closes.remove(index);
        }
    }

    pub(crate) fn cancel_workspace_drag(&mut self) -> Option<WorkspaceDrag> {
        self.workspace_drag.take()
    }

    pub(crate) fn register(&mut self, id: u64, handle: WindowHandle<FilerShell>) -> Option<u64> {
        if self.windows.contains_key(&id) {
            return None;
        }
        let incarnation = self.next_incarnation.checked_add(1)?;
        self.next_incarnation = incarnation;
        self.windows.insert(
            id,
            WindowRegistration {
                id,
                incarnation,
                handle,
            },
        );
        self.active_window.get_or_insert(id);
        Some(incarnation)
    }

    pub(crate) fn registrations(&self) -> Vec<WindowRegistration> {
        let mut windows = self.windows.values().cloned().collect::<Vec<_>>();
        windows.sort_by_key(|window| window.id);
        windows
    }

    pub(crate) fn unregister(&mut self, id: u64, incarnation: u64) -> bool {
        if self
            .windows
            .get(&id)
            .is_none_or(|window| window.incarnation != incarnation)
        {
            return false;
        }
        self.windows.remove(&id);
        if self.active_window == Some(id) {
            self.active_window = self.windows.keys().next().copied();
        }
        true
    }

    pub(crate) fn contains(&self, id: u64, incarnation: u64) -> bool {
        self.windows
            .get(&id)
            .is_some_and(|window| window.incarnation == incarnation)
    }

    pub(crate) fn active_window(&self) -> Option<u64> {
        self.active_window
    }

    pub(crate) fn set_active_window(&mut self, id: u64) {
        if self.windows.contains_key(&id) {
            self.active_window = Some(id);
        }
    }
}
