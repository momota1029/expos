use super::*;

pub(super) fn internal_move_request(
    sources: Vec<InternalDragSource>,
    target: InternalDropTarget,
    view_id: usize,
) -> OperationRequest {
    OperationRequest {
        kind: OperationKind::Move,
        virtual_source: None,
        sources: sources.iter().map(|source| source.path.clone()).collect(),
        destination: Some(target.path.clone()),
        move_identity_guard: Some(operations::model::MoveIdentityGuard {
            source_identities: sources
                .iter()
                .map(|source| source.identity.clone())
                .collect(),
            destination_identity: target.identity,
        }),
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
        origin: Some(OperationOrigin {
            view_id: view_id as u64,
            location_epoch: target.location_generation,
            presentation_revision: target.presentation_revision,
        }),
        clipboard_move_source_evidence: None,
        batch_rename_plan: None,
    }
}

pub(super) fn internal_background_move_request(
    sources: &[InternalDragSource],
    target: &ExternalDropTarget,
    view_id: usize,
) -> Option<OperationRequest> {
    let ExternalDropTarget::Background {
        path,
        identity: Some(identity),
        location_generation,
        presentation_revision,
    } = target
    else {
        return None;
    };
    if sources.is_empty()
        || !internal_drop_target_is_valid(sources, Some(path), EntryKind::Folder, false)
    {
        return None;
    }
    Some(OperationRequest {
        kind: OperationKind::Move,
        virtual_source: None,
        sources: sources.iter().map(|source| source.path.clone()).collect(),
        destination: Some(path.clone()),
        move_identity_guard: Some(operations::model::MoveIdentityGuard {
            source_identities: sources
                .iter()
                .map(|source| source.identity.clone())
                .collect(),
            destination_identity: identity.clone(),
        }),
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
        origin: Some(OperationOrigin {
            view_id: view_id as u64,
            location_epoch: *location_generation,
            presentation_revision: *presentation_revision,
        }),
        clipboard_move_source_evidence: None,
        batch_rename_plan: None,
    })
}

pub(super) fn navigation_move_request(
    sources: &[InternalDragSource],
    target: &ExternalDropTarget,
    target_tab_id: usize,
) -> Option<OperationRequest> {
    let ExternalDropTarget::NavigationDirectory {
        path,
        identity,
        location_generation,
        presentation_revision,
        ..
    } = target
    else {
        return None;
    };
    if sources.is_empty()
        || !internal_drop_target_is_valid(sources, Some(path), EntryKind::Folder, false)
    {
        return None;
    }
    Some(OperationRequest {
        kind: OperationKind::Move,
        virtual_source: None,
        sources: sources.iter().map(|source| source.path.clone()).collect(),
        destination: Some(path.clone()),
        move_identity_guard: Some(operations::model::MoveIdentityGuard {
            source_identities: sources
                .iter()
                .map(|source| source.identity.clone())
                .collect(),
            destination_identity: identity.clone(),
        }),
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
        origin: Some(OperationOrigin {
            view_id: target_tab_id as u64,
            location_epoch: *location_generation,
            presentation_revision: *presentation_revision,
        }),
        clipboard_move_source_evidence: None,
        batch_rename_plan: None,
    })
}

pub(super) fn refresh_navigation_target_snapshot(
    view: &DirectoryListDemo,
    rendered: &NavigationTargetSurfaceSnapshot,
) -> Option<NavigationTargetSurfaceSnapshot> {
    if view.current_location_generation() != rendered.location_generation
        || view.presentation_revision != rendered.presentation_revision
        || view
            .location()
            .and_then(|location| location.path().map(std::path::Path::to_path_buf))
            .as_deref()
            != Some(rendered.path.as_path())
    {
        return None;
    }

    let snapshot = view.capture_navigation_target_snapshot(
        rendered.view_entity_id,
        &rendered.path,
        rendered.require_current_location,
    );
    (snapshot.location_generation == rendered.location_generation
        && snapshot.presentation_revision == rendered.presentation_revision)
        .then_some(snapshot)
}

pub(super) fn external_copy_request(
    sources: Vec<std::path::PathBuf>,
    target: ExternalDropTarget,
    view_id: usize,
    kind: OperationKind,
) -> Option<OperationRequest> {
    if sources.is_empty() {
        return None;
    }
    let (
        destination,
        location_epoch,
        presentation_revision,
        destination_identity,
        copy_destination_identity_guard,
    ) = match target {
        ExternalDropTarget::Background {
            path,
            identity,
            location_generation,
            presentation_revision,
        } => (
            path,
            location_generation,
            presentation_revision,
            identity,
            None,
        ),
        ExternalDropTarget::Folder(target) => (
            target.path,
            target.location_generation,
            target.presentation_revision,
            Some(target.identity.clone()),
            Some(target.identity),
        ),
        ExternalDropTarget::NavigationDirectory {
            path,
            identity,
            location_generation,
            presentation_revision,
            ..
        } => (
            path,
            location_generation,
            presentation_revision,
            Some(identity.clone()),
            Some(identity),
        ),
    };
    if matches!(
        kind,
        OperationKind::Move | OperationKind::CreateSymbolicLink
    ) && destination_identity.is_none()
    {
        return None;
    }
    Some(OperationRequest {
        kind,
        virtual_source: None,
        sources,
        destination: Some(destination),
        move_identity_guard: None,
        clipboard_move_source_evidence: None,
        rename_reparse_guard: None,
        move_destination_identity_guard: (kind == OperationKind::Move)
            .then_some(destination_identity.clone())
            .flatten(),
        move_replace_source_evidence: None,
        trash_source_guards: None,
        source_cleanup_identity_guard: None,
        initial_conflict_policy: None,
        copy_destination_identity_guard: (kind == OperationKind::Copy)
            .then_some(copy_destination_identity_guard)
            .flatten(),
        create_parent_directory_identity_guard: (kind == OperationKind::CreateSymbolicLink)
            .then_some(destination_identity)
            .flatten(),
        zip_create_guard: None,
        shortcut_create_guard: None,
        seven_zip_create_guard: None,
        seven_zip_extract_guard: None,
        origin: Some(OperationOrigin {
            view_id: view_id as u64,
            location_epoch,
            presentation_revision,
        }),
        batch_rename_plan: None,
    })
}

pub(super) fn external_move_request(
    sources: Vec<std::path::PathBuf>,
    target: ExternalDropTarget,
    view_id: usize,
    source_identities: Vec<crate::filesystem_identity::FilesystemIdentity>,
) -> Option<OperationRequest> {
    if sources.len() != source_identities.len() {
        return None;
    }
    let mut request = external_copy_request(sources, target, view_id, OperationKind::Move)?;
    let destination_identity = request.move_destination_identity_guard.take()?;
    request.move_identity_guard = Some(operations::model::MoveIdentityGuard {
        source_identities,
        destination_identity,
    });
    Some(request)
}

pub(super) fn native_drag_pin_policy(
    snapshot: &InternalDragSnapshot,
) -> win_shell_dnd::FileDragPinPolicy {
    if snapshot.sources.iter().all(|source| !source.is_directory) {
        win_shell_dnd::FileDragPinPolicy::MoveCapable
    } else {
        win_shell_dnd::FileDragPinPolicy::CopyOnly
    }
}

pub(super) fn local_move_terminal_authorized(
    policy: win_shell_dnd::FileDragPinPolicy,
    execution: Option<&win_shell_dnd::DragExecution>,
) -> bool {
    // GPUI の local DropTarget は FileDropResponse を `pdwEffect` に返すだけで、
    // IDataObject::SetData(CFSTR_PERFORMEDDROPEFFECT) は呼ばない。後者は外部cleanup専用。
    let expected = match policy {
        win_shell_dnd::FileDragPinPolicy::MoveCapable => 2,
        win_shell_dnd::FileDragPinPolicy::CopyOnly => 1,
    };
    execution.is_some_and(|execution| {
        execution.result == win_shell_dnd::DragResult::Dropped
            && execution.performed_effect == Some(expected)
    })
}

pub(super) fn local_ole_effect_for_policy(
    policy: win_shell_dnd::FileDragPinPolicy,
) -> LocalOleDropEffect {
    match policy {
        win_shell_dnd::FileDragPinPolicy::MoveCapable => LocalOleDropEffect::Move,
        win_shell_dnd::FileDragPinPolicy::CopyOnly => LocalOleDropEffect::Copy,
    }
}

pub(super) fn native_drag_source_label(
    snapshot: &InternalDragSnapshot,
    translator: &Translator,
) -> String {
    if snapshot.sources.len() > 1 {
        return translator.item_count(snapshot.sources.len());
    }
    snapshot
        .sources
        .first()
        .and_then(|source| source.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| translator.item_count(snapshot.sources.len().max(1)))
}

pub(super) fn external_move_cleanup_request(
    snapshot: &InternalDragSnapshot,
    tab_id: usize,
    paths: Vec<std::path::PathBuf>,
    parent_routes: Vec<Vec<operations::model::SourceCleanupParentIdentity>>,
    leaf_signatures: Vec<operations::model::SourceCleanupLeafSignature>,
) -> Option<OperationRequest> {
    if snapshot.sources.is_empty()
        || snapshot.sources.iter().any(|source| source.is_directory)
        || paths.len() != snapshot.sources.len()
        || leaf_signatures.len() != snapshot.sources.len()
        || leaf_signatures
            .iter()
            .any(|signature| signature.number_of_links == 0)
        || paths
            .iter()
            .zip(&snapshot.sources)
            .any(|(path, source)| path != &source.path)
    {
        return None;
    }
    Some(OperationRequest {
        kind: OperationKind::ExternalMoveCleanup,
        virtual_source: None,
        sources: paths,
        destination: None,
        move_identity_guard: None,
        clipboard_move_source_evidence: None,
        rename_reparse_guard: None,
        move_destination_identity_guard: None,
        move_replace_source_evidence: None,
        trash_source_guards: None,
        source_cleanup_identity_guard: Some(operations::model::SourceCleanupIdentityGuard {
            source_identities: snapshot
                .sources
                .iter()
                .map(|source| source.identity.clone())
                .collect(),
            parent_routes,
            leaf_signatures,
        }),
        initial_conflict_policy: None,
        copy_destination_identity_guard: None,
        create_parent_directory_identity_guard: None,
        zip_create_guard: None,
        shortcut_create_guard: None,
        seven_zip_create_guard: None,
        seven_zip_extract_guard: None,
        origin: Some(OperationOrigin {
            view_id: tab_id as u64,
            location_epoch: snapshot.location_generation,
            presentation_revision: snapshot.presentation_revision,
        }),
        batch_rename_plan: None,
    })
}
