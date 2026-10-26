//! Custody-preserving staging and publication for operations that may not
//! silently discard a partially created or published directory tree.
use super::*;
use std::{fmt, path::PathBuf, sync::atomic::AtomicBool};

/// A root whose creation succeeded, even when reading its identity failed.
// IDが不明な作成済みrootも、明示的な後続判断まで破棄せずhandleごと返す。
pub(in crate::operations::executor) struct RetainedRoot {
    pub(in crate::operations::executor) root_path: PathBuf,
    pub(in crate::operations::executor) root_identity: Option<String>,
    pub(in crate::operations::executor) root_chain: Vec<DirectoryLock>,
    pub(in crate::operations::executor) root: DirectoryLock,
}

impl fmt::Debug for RetainedRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetainedRoot")
            .field("root_path", &self.root_path)
            .field("root_identity_known", &self.root_identity.is_some())
            .field("route_pins", &self.root_chain.len())
            .finish_non_exhaustive()
    }
}

pub(in crate::operations::executor) enum CreateRoot {
    BeforeCreate(CopyError),
    Ready(StagingRootLease),
    Created {
        error: CopyError,
        root: RetainedRoot,
    },
}

impl fmt::Debug for CreateRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCreate(error) => f.debug_tuple("BeforeCreate").field(error).finish(),
            Self::Ready(root) => f
                .debug_struct("Ready")
                .field("root_path", &root.root_path)
                .field("root_identity", &root.root_identity)
                .finish(),
            Self::Created { error, root } => f
                .debug_struct("Created")
                .field("error", error)
                .field("root", root)
                .finish(),
        }
    }
}

pub(in crate::operations::executor) fn create_root(root_path: &std::path::Path) -> CreateRoot {
    create_root_with_identity(root_path, directory_identity_text)
}

fn create_root_with_identity(
    root_path: &std::path::Path,
    identity_of: impl FnOnce(&DirectoryLock) -> Result<String, CopyError>,
) -> CreateRoot {
    let Some(parent_path) = root_path.parent() else {
        return CreateRoot::BeforeCreate(CopyError::Unsupported);
    };
    let Some(leaf) = root_path.file_name() else {
        return CreateRoot::BeforeCreate(CopyError::Unsupported);
    };
    if !root_path.is_absolute() {
        return CreateRoot::BeforeCreate(CopyError::Unsupported);
    }
    if let Err(error) = ensure_local_fixed_ntfs(parent_path) {
        return CreateRoot::BeforeCreate(error);
    }
    let mut root_chain = match open_staging_ancestor_chain(parent_path) {
        Ok(chain) => chain,
        Err(error) => return CreateRoot::BeforeCreate(error),
    };
    let parent = match open_staging_directory(parent_path) {
        Ok(parent) => parent,
        Err(error) => return CreateRoot::BeforeCreate(error),
    };
    root_chain.push(parent);
    let leaf = leaf.encode_wide().collect::<Vec<_>>();
    let handle = match nt_create_child(
        root_chain.last().expect("parent route just appended"),
        &leaf,
        true,
        true,
        FILE_SHARE_READ.0,
    ) {
        Ok(handle) => handle,
        Err(error) => return CreateRoot::BeforeCreate(error),
    };
    let root = DirectoryLock(handle);
    match identity_of(&root) {
        Ok(root_identity) => CreateRoot::Ready(StagingRootLease {
            root_path: root_path.to_path_buf(),
            root_identity,
            root_chain,
            root,
        }),
        Err(error) => CreateRoot::Created {
            error,
            root: RetainedRoot {
                root_path: root_path.to_path_buf(),
                root_identity: None,
                root_chain,
                root,
            },
        },
    }
}

#[cfg(test)]
pub(in crate::operations::executor) fn create_root_with_test_identity_failure(
    root_path: &std::path::Path,
) -> CreateRoot {
    create_root_with_identity(root_path, |_| {
        Err(CopyError::Io(
            "injected initial root identity failure".into(),
        ))
    })
}

pub(in crate::operations::executor) enum PublishRetainedFailure {
    BeforeRename {
        error: CopyError,
        staged: VerifiedStagedTree,
    },
    RenameAttempted {
        error: CopyError,
        outcome: RenameOutcome,
        retained: RetainedPublishedTree,
    },
}

impl fmt::Debug for PublishRetainedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeRename { error, staged } => f
                .debug_struct("BeforeRename")
                .field("error", error)
                .field("stage_path", &staged.root_path)
                .field("expected_nodes", &staged.nodes.len())
                .field("pinned_nodes", &staged.handles.len())
                .finish(),
            Self::RenameAttempted {
                error,
                outcome,
                retained,
            } => f
                .debug_struct("RenameAttempted")
                .field("error", error)
                .field("outcome", outcome)
                .field("retained", retained)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::operations::executor) enum RenameOutcome {
    ReportedError,
    Succeeded,
}

/// Complete filesystem facts survive all post-rename errors. Handles in the
/// partial projection identify what has already been reopened and checked.
// rename後はパスprobeから失敗を推定せず、全expected factを保持して照合を待つ。
pub(in crate::operations::executor) struct RetainedPublishedTree {
    pub(in crate::operations::executor) stage_path: PathBuf,
    pub(in crate::operations::executor) destination: PathBuf,
    pub(in crate::operations::executor) root_identity: String,
    pub(in crate::operations::executor) expected_nodes: Vec<StagedChildFact>,
    pub(in crate::operations::executor) partial: PartialPublishedTree,
}

impl fmt::Debug for RetainedPublishedTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetainedPublishedTree")
            .field("stage_path", &self.stage_path)
            .field("destination", &self.destination)
            .field("root_identity", &self.root_identity)
            .field("expected_nodes", &self.expected_nodes.len())
            .field("verified_nodes", &self.partial.nodes.len())
            .field("pending_node", &self.partial.pending_node.is_some())
            .finish()
    }
}

pub(in crate::operations::executor) fn publish(
    staged: VerifiedStagedTree,
    destination: &std::path::Path,
    cancelled: &AtomicBool,
) -> Result<VerifiedPublishedTree, PublishRetainedFailure> {
    publish_with_hooks(
        staged,
        destination,
        cancelled,
        publish_no_replace_handle,
        || {},
        open_published_root,
        |_| {},
    )
}

#[cfg(test)]
pub(in crate::operations::executor) fn publish_cancel_after_rename_for_test(
    staged: VerifiedStagedTree,
    destination: &std::path::Path,
    cancelled: &AtomicBool,
) -> Result<VerifiedPublishedTree, PublishRetainedFailure> {
    publish_with_hooks(
        staged,
        destination,
        cancelled,
        publish_no_replace_handle,
        || cancelled.store(true, Ordering::Release),
        open_published_root,
        |_| {},
    )
}

#[cfg(test)]
pub(in crate::operations::executor) fn publish_fail_root_reopen_after_rename_for_test(
    staged: VerifiedStagedTree,
    destination: &std::path::Path,
    cancelled: &AtomicBool,
) -> Result<VerifiedPublishedTree, PublishRetainedFailure> {
    publish_with_hooks(
        staged,
        destination,
        cancelled,
        publish_no_replace_handle,
        || {},
        |_| {
            Err(CopyError::Io(
                "injected published root reopen failure".into(),
            ))
        },
        |_| {},
    )
}

fn publish_with_hooks(
    mut staged: VerifiedStagedTree,
    destination: &std::path::Path,
    cancelled: &AtomicBool,
    rename: impl FnOnce(::windows::Win32::Foundation::HANDLE, &std::path::Path) -> Result<(), CopyError>,
    after_rename: impl FnOnce(),
    mut open_root: impl FnMut(&std::path::Path) -> Result<DirectoryLock, CopyError>,
    mut after_child_open: impl FnMut(bool),
) -> Result<VerifiedPublishedTree, PublishRetainedFailure> {
    if cancelled.load(Ordering::Acquire)
        || staged.root_path.parent() != destination.parent()
        || !destination.is_absolute()
        || destination.file_name().is_none()
    {
        return Err(PublishRetainedFailure::BeforeRename {
            error: if cancelled.load(Ordering::Acquire) {
                CopyError::Cancelled
            } else {
                CopyError::Unsupported
            },
            staged,
        });
    }
    let root_identity = match directory_identity(&staged.root) {
        Ok(identity) => identity,
        Err(error) => return Err(PublishRetainedFailure::BeforeRename { error, staged }),
    };
    let identity_text = format!(
        "{}:{:08x}{:08x}",
        root_identity.volume, root_identity.index_high, root_identity.index_low
    );
    if identity_text != staged.root_identity {
        return Err(PublishRetainedFailure::BeforeRename {
            error: CopyError::Unsupported,
            staged,
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(PublishRetainedFailure::BeforeRename {
            error: CopyError::Cancelled,
            staged,
        });
    }

    let stage_path = staged.root_path.clone();
    let expected_nodes = std::mem::take(&mut staged.nodes);
    // 子handleは親root renameと両立しないので、事実を別に保持して先に解放する。
    drop(std::mem::take(&mut staged.handles));
    let rename_result = rename(staged.root.0, destination);
    let (outcome, error) = match rename_result {
        Ok(()) => (RenameOutcome::Succeeded, None),
        Err(error) => (RenameOutcome::ReportedError, Some(error)),
    };
    let partial = PartialPublishedTree {
        root_path: destination.to_path_buf(),
        root_identity: staged.root_identity.clone(),
        root_chain: staged.root_chain,
        root_publish_handle: staged.root,
        reopened_root: None,
        nodes: Vec::with_capacity(expected_nodes.len()),
        _staged_handles: Vec::new(),
        pending_node: None,
    };
    let mut retained = RetainedPublishedTree {
        stage_path,
        destination: destination.to_path_buf(),
        root_identity: staged.root_identity,
        expected_nodes,
        partial,
    };
    if let Some(error) = error {
        return Err(PublishRetainedFailure::RenameAttempted {
            error,
            outcome,
            retained,
        });
    }

    after_rename();
    let result = verify_published_tree_after_publish(
        &retained.expected_nodes,
        root_identity,
        cancelled,
        |path| open_root(path),
        &mut after_child_open,
        &mut retained.partial,
    );
    match result {
        Ok(()) if retained.partial.pending_node.is_none() => {
            if let Some(root) = retained.partial.reopened_root.take() {
                Ok(VerifiedPublishedTree {
                    root_path: retained.partial.root_path,
                    root_identity: retained.partial.root_identity,
                    root_chain: retained.partial.root_chain,
                    root_publish_handle: retained.partial.root_publish_handle,
                    root,
                    nodes: retained.partial.nodes,
                })
            } else {
                Err(PublishRetainedFailure::RenameAttempted {
                    error: CopyError::Unsupported,
                    outcome,
                    retained,
                })
            }
        }
        Ok(()) => Err(PublishRetainedFailure::RenameAttempted {
            error: CopyError::Unsupported,
            outcome,
            retained,
        }),
        Err(error) => Err(PublishRetainedFailure::RenameAttempted {
            error,
            outcome,
            retained,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

    fn test_parent(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let parent = std::env::temp_dir().join(format!(
            "gpui-filer-retained-tree-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&parent).expect("create test parent");
        parent
    }

    fn empty_stage(path: &std::path::Path) -> VerifiedStagedTree {
        let CreateRoot::Ready(root) = create_root(path) else {
            panic!("test staging root creation failed")
        };
        VerifiedStagedTree {
            root_path: root.root_path,
            root_identity: root.root_identity,
            root_chain: root.root_chain,
            root: root.root,
            nodes: Vec::new(),
            handles: Vec::new(),
        }
    }

    fn staged_with_files(parent: &std::path::Path, files: &[(&str, &[u8])]) -> VerifiedStagedTree {
        let source_path = parent.join("source-tree");
        fs::create_dir(&source_path).expect("source root");
        for (name, contents) in files {
            fs::write(source_path.join(name), contents).expect("source file");
        }
        let cancelled = AtomicBool::new(false);
        let mut source =
            preflight_pinned_source_tree(&source_path, &cancelled).expect("pin source tree");
        release_pinned_route(&mut source);
        let stage_path = parent.join("stage");
        let CreateRoot::Ready(root) = create_root(&stage_path) else {
            panic!("create staging root")
        };
        stage_pinned_source_tree(&mut source, root, &cancelled).expect("stage source tree")
    }

    fn staged_with_file(parent: &std::path::Path) -> VerifiedStagedTree {
        staged_with_files(parent, &[("sentinel", b"staged sentinel")])
    }

    fn facts_snapshot(
        facts: &[StagedChildFact],
    ) -> Vec<(
        Vec<Vec<u16>>,
        crate::operations::journal::JournalDirectoryNodeKind,
        String,
        String,
    )> {
        facts
            .iter()
            .map(|fact| {
                (
                    fact.relative_components.clone(),
                    fact.node_kind,
                    fact.identity.clone(),
                    fact.content_fact.clone(),
                )
            })
            .collect()
    }

    fn assert_snapshot_equal(
        expected: &[(
            Vec<Vec<u16>>,
            crate::operations::journal::JournalDirectoryNodeKind,
            String,
            String,
        )],
        actual: &[StagedChildFact],
    ) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.relative_components, expected.0);
            assert_eq!(actual.node_kind, expected.1);
            assert_eq!(actual.identity, expected.2);
            assert_eq!(actual.content_fact, expected.3);
        }
    }

    fn assert_verified_is_expected(expected: &[StagedChildFact], verified: &[VerifiedStagedNode]) {
        for node in verified {
            assert!(expected.iter().any(|fact| {
                fact.relative_components == node.relative_components
                    && fact.node_kind == node.node_kind
                    && fact.identity == node.identity
                    && fact.content_fact == node.content_fact
            }));
        }
    }

    #[test]
    fn identity_failure_returns_created_root_and_drop_does_not_delete_it() {
        let parent = test_parent("identity-error");
        let root_path = parent.join("stage");
        let created = create_root_with_identity(&root_path, |_| {
            Err(CopyError::Io("injected identity failure".into()))
        });
        let CreateRoot::Created { root, .. } = created else {
            panic!("creation must retain a root after identity failure")
        };
        assert!(root.root_path.exists());
        assert_eq!(root.root_identity, None);
        assert!(fs::rename(&root_path, parent.join("renamed")).is_err());
        drop(root);
        assert!(
            root_path.is_dir(),
            "drop must not implicitly dispose the root"
        );
        fs::remove_dir_all(parent).expect("remove fixture after handles drop");
    }

    #[test]
    fn native_create_collision_does_not_claim_foreign_root_and_valid_control_works() {
        let parent = test_parent("create-collision");
        let foreign_path = parent.join("foreign-root");
        fs::create_dir(&foreign_path).expect("foreign root");
        fs::write(foreign_path.join("sentinel"), b"foreign bytes").expect("foreign sentinel");
        let mut identity_called = false;
        let collision = create_root_with_identity(&foreign_path, |_| {
            identity_called = true;
            Ok("must not be queried".into())
        });
        assert!(matches!(collision, CreateRoot::BeforeCreate(_)));
        assert!(
            !identity_called,
            "identity lookup follows successful native creation"
        );
        assert_eq!(
            fs::read(foreign_path.join("sentinel")).unwrap(),
            b"foreign bytes"
        );

        let missing_parent_path = parent.join("missing").join("stage");
        assert!(matches!(
            create_root(&missing_parent_path),
            CreateRoot::BeforeCreate(_)
        ));
        assert!(!missing_parent_path.exists());

        let valid_path = parent.join("valid-root");
        let CreateRoot::Ready(valid) = create_root(&valid_path) else {
            panic!("valid sibling creation control")
        };
        assert!(valid_path.is_dir());
        drop(valid);
        assert_eq!(
            fs::read(foreign_path.join("sentinel")).unwrap(),
            b"foreign bytes"
        );
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn identified_root_pin_prevents_path_reuse_while_custody_is_held() {
        let parent = test_parent("root-pinned");
        let task_ancestor = parent.join("task-ancestor");
        fs::create_dir(&task_ancestor).expect("task ancestor");
        let root_path = task_ancestor.join("stage");
        let CreateRoot::Ready(root) = create_root(&root_path) else {
            panic!("create identified staging root")
        };
        assert!(root.root_identity.len() > 0);
        fs::write(root_path.join("sentinel"), b"root sentinel").expect("root sentinel");
        let ancestor_handle = root.root_chain.last().expect("root parent route handle");
        let ancestor_identity = directory_identity_text(ancestor_handle).unwrap();
        assert!(fs::rename(&root_path, parent.join("renamed")).is_err());
        assert!(fs::rename(&task_ancestor, parent.join("ancestor-replacement")).is_err());
        assert!(fs::rename(&parent, parent.with_extension("replacement")).is_err());
        assert!(root_path.is_dir());
        assert_eq!(
            fs::read(root_path.join("sentinel")).unwrap(),
            b"root sentinel"
        );
        assert_eq!(
            directory_identity_text(ancestor_handle).unwrap(),
            ancestor_identity
        );
        drop(root);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn cancelled_publish_keeps_stage_and_all_pins_before_rename() {
        let parent = test_parent("cancel");
        let stage_path = parent.join("stage");
        let destination = parent.join("result");
        let staged = staged_with_file(&parent);
        assert_eq!(staged.root_path, stage_path);
        assert!(!staged.handles.is_empty());
        let original_handle_count = staged.handles.len();
        let cancelled = AtomicBool::new(true);
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |_, _| panic!("rename must not run"),
            || {},
            open_published_root,
            |_| {},
        );
        let Err(PublishRetainedFailure::BeforeRename { staged, .. }) = result else {
            panic!("cancellation must return intact staged ownership")
        };
        assert!(stage_path.is_dir());
        assert_eq!(staged.handles.len(), original_handle_count);
        assert_eq!(
            fs::read(stage_path.join("sentinel")).unwrap(),
            b"staged sentinel"
        );
        assert_eq!(staged.root_identity.len() > 0, true);
        drop(staged);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn invalid_sibling_destination_returns_exact_stage_without_rename() {
        let parent = test_parent("invalid-sibling");
        let staged = staged_with_file(&parent);
        let stage_path = staged.root_path.clone();
        let expected_facts = facts_snapshot(&staged.nodes);
        let root_identity = staged.root_identity.clone();
        let route_count = staged.root_chain.len();
        let handle_count = staged.handles.len();
        let invalid_sibling = parent.join("other-parent").join("result");
        let cancelled = AtomicBool::new(false);
        let mut rename_calls = 0;
        let result = publish_with_hooks(
            staged,
            &invalid_sibling,
            &cancelled,
            |_, _| {
                rename_calls += 1;
                Ok(())
            },
            || {},
            open_published_root,
            |_| {},
        );
        let Err(PublishRetainedFailure::BeforeRename { staged, .. }) = result else {
            panic!("non-sibling destination must preserve stage before rename")
        };
        assert_eq!(rename_calls, 0);
        assert_eq!(staged.root_path, stage_path);
        assert_eq!(staged.root_identity, root_identity);
        assert_eq!(staged.root_chain.len(), route_count);
        assert_eq!(staged.handles.len(), handle_count);
        assert_snapshot_equal(&expected_facts, &staged.nodes);
        assert_eq!(
            fs::read(stage_path.join("sentinel")).unwrap(),
            b"staged sentinel"
        );
        drop(staged);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn root_identity_mismatch_returns_exact_stage_without_rename() {
        let parent = test_parent("root-identity-mismatch");
        let mut staged = staged_with_file(&parent);
        let stage_path = staged.root_path.clone();
        let expected_facts = facts_snapshot(&staged.nodes);
        let route_count = staged.root_chain.len();
        let handle_count = staged.handles.len();
        staged.root_identity.push_str(":substituted");
        let rejected_identity = staged.root_identity.clone();
        let cancelled = AtomicBool::new(false);
        let mut rename_calls = 0;
        let result = publish_with_hooks(
            staged,
            &parent.join("result"),
            &cancelled,
            |_, _| {
                rename_calls += 1;
                Ok(())
            },
            || {},
            open_published_root,
            |_| {},
        );
        let Err(PublishRetainedFailure::BeforeRename { staged, .. }) = result else {
            panic!("captured root identity mismatch must remain pre-rename")
        };
        assert_eq!(rename_calls, 0);
        assert_eq!(staged.root_path, stage_path);
        assert_eq!(staged.root_identity, rejected_identity);
        assert_eq!(staged.root_chain.len(), route_count);
        assert_eq!(staged.handles.len(), handle_count);
        assert_snapshot_equal(&expected_facts, &staged.nodes);
        assert_eq!(
            fs::read(stage_path.join("sentinel")).unwrap(),
            b"staged sentinel"
        );
        drop(staged);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn error_after_successful_native_rename_retains_facts_and_both_paths() {
        let parent = test_parent("rename-error-after-success");
        let stage_path = parent.join("stage");
        let destination = parent.join("result");
        let staged = staged_with_file(&parent);
        let expected_facts = facts_snapshot(&staged.nodes);
        let cancelled = AtomicBool::new(false);
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| {
                publish_no_replace_handle(handle, path)?;
                Err(CopyError::Io("injected error after native success".into()))
            },
            || {},
            open_published_root,
            |_| {},
        );
        let Err(PublishRetainedFailure::RenameAttempted {
            outcome, retained, ..
        }) = result
        else {
            panic!("post-attempt error must retain publication uncertainty")
        };
        assert_eq!(outcome, RenameOutcome::ReportedError);
        assert!(!stage_path.exists());
        assert!(destination.is_dir());
        assert_eq!(retained.stage_path, stage_path);
        assert_eq!(retained.destination, destination);
        assert_snapshot_equal(&expected_facts, &retained.expected_nodes);
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"staged sentinel"
        );
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn destination_collision_retains_stage_and_foreign_directory() {
        let parent = test_parent("collision");
        let stage_path = parent.join("stage");
        let destination = parent.join("result");
        let staged = staged_with_file(&parent);
        let expected_facts = facts_snapshot(&staged.nodes);
        fs::create_dir(&destination).expect("foreign destination");
        fs::write(destination.join("sentinel"), b"foreign").expect("foreign sentinel");
        let cancelled = AtomicBool::new(false);
        let result = publish(staged, &destination, &cancelled);
        let Err(PublishRetainedFailure::RenameAttempted { retained, .. }) = result else {
            panic!("native collision is a rename-attempt uncertainty")
        };
        assert!(stage_path.is_dir());
        assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"foreign");
        assert_snapshot_equal(&expected_facts, &retained.expected_nodes);
        assert_eq!(
            fs::read(stage_path.join("sentinel")).unwrap(),
            b"staged sentinel"
        );
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn postrename_cancellation_keeps_complete_expected_facts() {
        let parent = test_parent("postrename-cancel");
        let destination = parent.join("result");
        let staged = staged_with_file(&parent);
        let expected_count = staged.nodes.len();
        let cancelled = AtomicBool::new(false);
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| publish_no_replace_handle(handle, path),
            || {},
            open_published_root,
            |_| cancelled.store(true, Ordering::Release),
        );
        let Err(PublishRetainedFailure::RenameAttempted { retained, .. }) = result else {
            panic!("postrename cancellation retains the partial tree")
        };
        assert_eq!(retained.expected_nodes.len(), expected_count);
        assert!(retained.partial.nodes.is_empty());
        assert!(retained.partial.pending_node.is_some());
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn immediate_postrename_cancellation_skips_reopen_and_retains_expected_facts() {
        let parent = test_parent("immediate-cancel");
        let staged = staged_with_file(&parent);
        let expected_facts = facts_snapshot(&staged.nodes);
        let root_identity = staged.root_identity.clone();
        let route_count = staged.root_chain.len();
        let destination = parent.join("result");
        let cancelled = AtomicBool::new(false);
        let mut reopen_calls = 0;
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| publish_no_replace_handle(handle, path),
            || cancelled.store(true, Ordering::Release),
            |_| {
                reopen_calls += 1;
                open_published_root(&destination)
            },
            |_| {},
        );
        let Err(PublishRetainedFailure::RenameAttempted {
            outcome, retained, ..
        }) = result
        else {
            panic!("immediate postrename cancellation must retain uncertainty")
        };
        assert_eq!(outcome, RenameOutcome::Succeeded);
        assert_eq!(reopen_calls, 0);
        assert_eq!(retained.root_identity, root_identity);
        assert_snapshot_equal(&expected_facts, &retained.expected_nodes);
        assert_eq!(retained.partial.root_chain.len(), route_count);
        assert!(retained.partial.reopened_root.is_none());
        assert!(retained.partial.pending_node.is_none());
        assert!(destination.is_dir());
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn root_reopen_error_after_native_success_retains_root_and_expected_facts() {
        let parent = test_parent("root-reopen-error");
        let staged = staged_with_file(&parent);
        let expected_facts = facts_snapshot(&staged.nodes);
        let root_identity = staged.root_identity.clone();
        let route_count = staged.root_chain.len();
        let destination = parent.join("result");
        let cancelled = AtomicBool::new(false);
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| publish_no_replace_handle(handle, path),
            || {},
            |_| Err(CopyError::Io("injected root reopen failure".into())),
            |_| {},
        );
        let Err(PublishRetainedFailure::RenameAttempted {
            outcome, retained, ..
        }) = result
        else {
            panic!("root reopen error after native success must retain evidence")
        };
        assert_eq!(outcome, RenameOutcome::Succeeded);
        assert_eq!(retained.root_identity, root_identity);
        assert_snapshot_equal(&expected_facts, &retained.expected_nodes);
        assert_eq!(retained.partial.root_chain.len(), route_count);
        assert!(retained.partial.reopened_root.is_none());
        assert!(retained.partial.pending_node.is_none());
        assert!(destination.is_dir());
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"staged sentinel"
        );
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn multi_node_cancellation_retains_verified_pending_and_unopened_facts() {
        let parent = test_parent("multi-node-partial");
        let staged = staged_with_files(
            &parent,
            &[("a", b"first"), ("b", b"second"), ("c", b"third")],
        );
        let expected_facts = facts_snapshot(&staged.nodes);
        assert_eq!(expected_facts.len(), 3);
        let destination = parent.join("result");
        let cancelled = AtomicBool::new(false);
        let mut opened = 0;
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| publish_no_replace_handle(handle, path),
            || {},
            open_published_root,
            |_| {
                opened += 1;
                if opened == 2 {
                    // Hook runs after the child handle opens and before identity/hash checks.
                    cancelled.store(true, Ordering::Release);
                }
            },
        );
        let Err(PublishRetainedFailure::RenameAttempted { retained, .. }) = result else {
            panic!("multi-node cancellation must return a retained partial tree")
        };
        assert_eq!(retained.expected_nodes.len(), 3);
        assert_eq!(retained.partial.nodes.len(), 1);
        assert_verified_is_expected(&retained.expected_nodes, &retained.partial.nodes);
        let pending = retained
            .partial
            .pending_node
            .as_ref()
            .expect("pending child pin");
        assert!(retained.expected_nodes.iter().any(|fact| {
            fact.relative_components == pending.relative_components
                && fact.node_kind == pending.node_kind
                && fact.identity == pending.identity
                && fact.content_fact == pending.content_fact
        }));
        assert!(
            retained.expected_nodes.iter().any(|fact| {
                fact.relative_components != pending.relative_components
                    && !retained
                        .partial
                        .nodes
                        .iter()
                        .any(|node| node.relative_components == fact.relative_components)
            }),
            "third expected child remains unopened"
        );

        let pending_name = String::from_utf16_lossy(
            pending
                .relative_components
                .first()
                .expect("root child name"),
        );
        assert!(
            fs::write(destination.join(pending_name), b"external write").is_err(),
            "pending child handle blocks an external write"
        );
        assert!(
            fs::rename(&destination, parent.join("replacement-root")).is_err(),
            "exact root handle blocks a path-reuse rename"
        );
        assert_snapshot_equal(&expected_facts, &retained.expected_nodes);
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn child_content_mutation_in_pin_release_window_is_retained_as_partial() {
        let parent = test_parent("child-mutation");
        let stage_path = parent.join("stage");
        let destination = parent.join("result");
        let staged = staged_with_file(&parent);
        let expected_count = staged.nodes.len();
        let cancelled = AtomicBool::new(false);
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| {
                fs::write(stage_path.join("sentinel"), b"mutated after pins released")
                    .expect("mutate released child");
                publish_no_replace_handle(handle, path)
            },
            || {},
            open_published_root,
            |_| {},
        );
        let Err(PublishRetainedFailure::RenameAttempted { retained, .. }) = result else {
            panic!("content mutation must fail exact postpublish verification")
        };
        assert_eq!(retained.expected_nodes.len(), expected_count);
        assert!(retained.partial.pending_node.is_some());
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"mutated after pins released"
        );
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn reparse_substitution_in_pin_release_window_is_rejected_and_retained() {
        let parent = test_parent("reparse-substitution");
        let stage_path = parent.join("stage");
        let source_path = parent.join("source");
        fs::create_dir(&source_path).expect("source root");
        fs::create_dir(source_path.join("folder")).expect("source folder");
        fs::write(source_path.join("folder/file"), b"child").expect("source child");
        let cancelled = AtomicBool::new(false);
        let mut source =
            preflight_pinned_source_tree(&source_path, &cancelled).expect("pin source");
        release_pinned_route(&mut source);
        let CreateRoot::Ready(root) = create_root(&stage_path) else {
            panic!("create stage")
        };
        let staged = stage_pinned_source_tree(&mut source, root, &cancelled).expect("stage tree");
        let expected_count = staged.nodes.len();
        let destination = parent.join("result");
        let target = parent.join("junction-target");
        fs::create_dir(&target).expect("junction target");
        fs::write(target.join("foreign"), b"target sentinel").expect("target sentinel");
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| {
                fs::remove_dir_all(stage_path.join("folder")).expect("remove released folder");
                crate::operations::privileged::create_junction_direct(
                    &target,
                    &stage_path.join("folder"),
                )
                .expect("create nonprivileged test junction");
                publish_no_replace_handle(handle, path)
            },
            || {},
            open_published_root,
            |_| {},
        );
        let Err(PublishRetainedFailure::RenameAttempted { retained, .. }) = result else {
            panic!("reparse substitution must reject exact postpublish verification")
        };
        assert_eq!(retained.expected_nodes.len(), expected_count);
        assert!(retained.partial.pending_node.is_some());
        assert!(destination.join("folder").exists());
        assert_eq!(
            fs::read(target.join("foreign")).unwrap(),
            b"target sentinel"
        );
        drop(retained);
        drop(source);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn successful_publish_verifies_nested_tree_empty_file_and_directory() {
        let parent = test_parent("nested-success");
        let source_path = parent.join("source");
        fs::create_dir(&source_path).expect("source root");
        fs::create_dir(source_path.join("nested")).expect("nested directory");
        fs::create_dir(source_path.join("empty-directory")).expect("empty directory");
        fs::write(source_path.join("empty-file"), b"").expect("empty file");
        fs::write(source_path.join("nested/data"), b"verified bytes").expect("nested file");

        let cancelled = AtomicBool::new(false);
        let mut source =
            preflight_pinned_source_tree(&source_path, &cancelled).expect("pin source tree");
        release_pinned_route(&mut source);
        let stage_path = parent.join("stage");
        let root = match create_root(&stage_path) {
            CreateRoot::Ready(root) => root,
            result => panic!("create staging root: {result:?}"),
        };
        let staged =
            stage_pinned_source_tree(&mut source, root, &cancelled).expect("stage source tree");
        let expected = staged.nodes.len();
        let destination = parent.join("published");
        let published = publish(staged, &destination, &cancelled).expect("publish and verify");
        assert_eq!(published.nodes.len(), expected);
        assert!(destination.join("empty-directory").is_dir());
        assert_eq!(fs::read(destination.join("empty-file")).unwrap(), b"");
        assert_eq!(
            fs::read(destination.join("nested/data")).unwrap(),
            b"verified bytes"
        );
        drop(published);
        drop(source);
        fs::remove_dir_all(parent).expect("remove fixture");
    }

    #[test]
    fn substituted_destination_identity_never_becomes_success() {
        let parent = test_parent("identity-substitution");
        let stage_path = parent.join("stage");
        let destination = parent.join("result");
        let foreign = parent.join("foreign");
        fs::create_dir(&foreign).expect("foreign root");
        let staged = empty_stage(&stage_path);
        let cancelled = AtomicBool::new(false);
        let result = publish_with_hooks(
            staged,
            &destination,
            &cancelled,
            |handle, path| publish_no_replace_handle(handle, path),
            || {},
            |_| open_published_root(&foreign),
            |_| {},
        );
        let Err(PublishRetainedFailure::RenameAttempted { retained, .. }) = result else {
            panic!("a substituted destination root cannot verify as success")
        };
        assert!(destination.is_dir());
        assert!(retained.partial.reopened_root.is_some());
        assert_eq!(
            directory_identity_text(&retained.partial.root_publish_handle).unwrap(),
            retained.root_identity
        );
        drop(retained);
        fs::remove_dir_all(parent).expect("remove fixture");
    }
}
