//! Parent-owned materialization of validated payload-spool ranges.
use super::*;
use crate::{
    archive_manifest::NodeKind,
    archive_provider::Validated7zExtractPlan,
    operations::journal::{JournalDirectoryNodeKind, JournalSevenZipExtractIdentity},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::io::FromRawHandle,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checkpoint {
    BeforePreflight,
    BeforeChild(usize),
    BeforeWrite(usize),
    AfterWrite(usize),
    AfterSync(usize),
    BeforeFinalSpoolValidation,
    BeforeExactNamespaceValidation,
    BeforeSuccess,
}

trait MaterializeIo {
    fn child_identity(
        &mut self,
        index: usize,
        child: &PinnedSourceHandle,
    ) -> Result<String, CopyError>;
    fn read_spool(
        &mut self,
        spool: &mut std::fs::File,
        buffer: &mut [u8],
    ) -> std::io::Result<usize>;
    fn write_child(
        &mut self,
        index: usize,
        file: &mut std::fs::File,
        bytes: &[u8],
    ) -> std::io::Result<usize>;
    fn sync_child(&mut self, index: usize, file: &std::fs::File) -> std::io::Result<()>;
    fn checkpoint(
        &mut self,
        point: Checkpoint,
        cancelled: &AtomicBool,
        deadline: Instant,
        root: &DirectoryLock,
        root_path: &Path,
    ) -> Result<(), CopyError>;
}

struct NativeIo;

impl MaterializeIo for NativeIo {
    fn child_identity(
        &mut self,
        _index: usize,
        child: &PinnedSourceHandle,
    ) -> Result<String, CopyError> {
        match child {
            PinnedSourceHandle::Directory(dir) => directory_identity_text(dir),
            PinnedSourceHandle::File(file) => file_identity_text(file),
        }
    }
    fn read_spool(
        &mut self,
        spool: &mut std::fs::File,
        buffer: &mut [u8],
    ) -> std::io::Result<usize> {
        spool.read(buffer)
    }
    fn write_child(
        &mut self,
        _index: usize,
        file: &mut std::fs::File,
        bytes: &[u8],
    ) -> std::io::Result<usize> {
        file.write(bytes)
    }
    fn sync_child(&mut self, _index: usize, file: &std::fs::File) -> std::io::Result<()> {
        file.sync_all()
    }
    fn checkpoint(
        &mut self,
        _point: Checkpoint,
        cancelled: &AtomicBool,
        deadline: Instant,
        _root: &DirectoryLock,
        _root_path: &Path,
    ) -> Result<(), CopyError> {
        native_checkpoint(cancelled, deadline)
    }
}

#[cfg(test)]
struct StopAfterFirstWriteIo {
    io: NativeIo,
    expire_deadline: bool,
}

#[cfg(test)]
impl MaterializeIo for StopAfterFirstWriteIo {
    fn child_identity(
        &mut self,
        index: usize,
        child: &PinnedSourceHandle,
    ) -> Result<String, CopyError> {
        self.io.child_identity(index, child)
    }

    fn read_spool(
        &mut self,
        spool: &mut std::fs::File,
        buffer: &mut [u8],
    ) -> std::io::Result<usize> {
        self.io.read_spool(spool, buffer)
    }

    fn write_child(
        &mut self,
        index: usize,
        file: &mut std::fs::File,
        bytes: &[u8],
    ) -> std::io::Result<usize> {
        self.io.write_child(index, file, bytes)
    }

    fn sync_child(&mut self, index: usize, file: &std::fs::File) -> std::io::Result<()> {
        self.io.sync_child(index, file)
    }

    fn checkpoint(
        &mut self,
        point: Checkpoint,
        cancelled: &AtomicBool,
        deadline: Instant,
        root: &DirectoryLock,
        root_path: &Path,
    ) -> Result<(), CopyError> {
        if matches!(point, Checkpoint::AfterWrite(_)) && self.expire_deadline {
            // 実際の書込み後に期限切れInstantでproductionのcheckpointを通す。
            return self.io.checkpoint(
                point,
                cancelled,
                Instant::now() - std::time::Duration::from_millis(1),
                root,
                root_path,
            );
        }
        if matches!(point, Checkpoint::AfterWrite(_)) {
            // 実際のchild write直後にcancelし、productionと同じcheckpointで止める。
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        }
        self.io
            .checkpoint(point, cancelled, deadline, root, root_path)
    }
}

pub(in crate::operations::executor) struct RetainedStagedChild {
    pub(in crate::operations::executor) node_index: usize,
    pub(in crate::operations::executor) handle: PinnedSourceHandle,
    pub(in crate::operations::executor) observed_identity: Option<String>,
    pub(in crate::operations::executor) content_fact: Option<String>,
    pub(in crate::operations::executor) copied_bytes: u64,
    relative_components: Vec<Vec<u16>>,
    node_kind: JournalDirectoryNodeKind,
}

pub(in crate::operations::executor) struct RetainedSpoolTree {
    pub(in crate::operations::executor) root: StagingRootLease,
    pub(in crate::operations::executor) plan: Arc<Validated7zExtractPlan>,
    pub(in crate::operations::executor) children: Vec<RetainedStagedChild>,
    pub(in crate::operations::executor) copied_bytes: u64,
}

pub(in crate::operations::executor) struct CleanupChildProjection {
    pub(in crate::operations::executor) relative_components: Vec<Vec<u16>>,
    pub(in crate::operations::executor) node_kind: JournalDirectoryNodeKind,
    pub(in crate::operations::executor) observed_identity: Option<String>,
    pub(in crate::operations::executor) handle: PinnedSourceHandle,
}

pub(in crate::operations::executor) fn into_cleanup_projection(
    retained: RetainedSpoolTree,
) -> (StagingRootLease, Vec<CleanupChildProjection>) {
    let RetainedSpoolTree { root, children, .. } = retained;
    let children = children
        .into_iter()
        .map(|child| CleanupChildProjection {
            relative_components: child.relative_components,
            node_kind: child.node_kind,
            observed_identity: child.observed_identity,
            handle: child.handle,
        })
        .collect();
    (root, children)
}

#[cfg(test)]
pub(super) fn retained_tree_for_cleanup_test(
    root: StagingRootLease,
    name: &str,
    handle: PinnedSourceHandle,
    node_kind: JournalDirectoryNodeKind,
    observed_identity: Option<String>,
) -> RetainedSpoolTree {
    RetainedSpoolTree {
        root,
        plan: Arc::new(
            Validated7zExtractPlan::validate(Vec::new()).expect("empty test plan is valid"),
        ),
        children: vec![RetainedStagedChild {
            node_index: 0,
            handle,
            observed_identity,
            content_fact: None,
            copied_bytes: 0,
            relative_components: vec![name.encode_utf16().collect()],
            node_kind,
        }],
        copied_bytes: 0,
    }
}

impl fmt::Debug for RetainedSpoolTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetainedSpoolTree")
            .field("root_path", &self.root.root_path)
            .field("root_identity_known", &true)
            .field("children", &self.children.len())
            .field("copied_bytes", &self.copied_bytes)
            .finish()
    }
}

pub(in crate::operations::executor) struct SpoolTreeFailure {
    pub(in crate::operations::executor) error: CopyError,
    // 失敗時の所有権は保ったまま、可変長inventoryでResult型を大きくしない。
    pub(in crate::operations::executor) retained: Box<RetainedSpoolTree>,
}

impl fmt::Debug for SpoolTreeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpoolTreeFailure")
            .field("error", &self.error)
            .field("retained", &*self.retained)
            .finish()
    }
}

pub(in crate::operations::executor) fn materialize(
    spool: &mut std::fs::File,
    expected_spool: JournalSevenZipExtractIdentity,
    plan: Arc<Validated7zExtractPlan>,
    root: StagingRootLease,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<(VerifiedStagedTree, u64), SpoolTreeFailure> {
    // 呼び出し側はSpoolReady/TreeOwnedの正確な永続ACK、最終decode receipt、helper Job全体のdrainを先に確定し、provider/source routeを保持し続けること。
    // 複製spool handleによる同時I/Oとroot namespaceの同時変更も排除する。&mut Fileや期待identityだけではACK・drain・排他の証明にならない。
    // この同期I/Oはworker threadで実行し、deadlineは各境界で観測する協調的な期限であり、停止中のnative呼び出しを中断しない。
    materialize_with_io(
        spool,
        expected_spool,
        plan,
        root,
        cancelled,
        deadline,
        &mut NativeIo,
    )
}

#[cfg(test)]
pub(in crate::operations::executor) fn materialize_cancel_after_first_write_for_test(
    spool: &mut std::fs::File,
    expected_spool: JournalSevenZipExtractIdentity,
    plan: Arc<Validated7zExtractPlan>,
    root: StagingRootLease,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<(VerifiedStagedTree, u64), SpoolTreeFailure> {
    materialize_with_io(
        spool,
        expected_spool,
        plan,
        root,
        cancelled,
        deadline,
        &mut StopAfterFirstWriteIo {
            io: NativeIo,
            expire_deadline: false,
        },
    )
}

#[cfg(test)]
pub(in crate::operations::executor) fn materialize_expire_after_first_write_for_test(
    spool: &mut std::fs::File,
    expected_spool: JournalSevenZipExtractIdentity,
    plan: Arc<Validated7zExtractPlan>,
    root: StagingRootLease,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<(VerifiedStagedTree, u64), SpoolTreeFailure> {
    materialize_with_io(
        spool,
        expected_spool,
        plan,
        root,
        cancelled,
        deadline,
        &mut StopAfterFirstWriteIo {
            io: NativeIo,
            expire_deadline: true,
        },
    )
}

fn materialize_with_io(
    spool: &mut std::fs::File,
    expected_spool: JournalSevenZipExtractIdentity,
    plan: Arc<Validated7zExtractPlan>,
    root: StagingRootLease,
    cancelled: &AtomicBool,
    deadline: Instant,
    io: &mut impl MaterializeIo,
) -> Result<(VerifiedStagedTree, u64), SpoolTreeFailure> {
    let mut retained = RetainedSpoolTree {
        root,
        plan,
        children: Vec::new(),
        copied_bytes: 0,
    };
    let result = materialize_inner(
        spool,
        expected_spool,
        &mut retained,
        cancelled,
        deadline,
        io,
    );
    match result {
        Ok(()) => {
            let RetainedSpoolTree {
                root,
                plan: _,
                children,
                copied_bytes,
            } = retained;
            let nodes = children
                .iter()
                .map(|child| StagedChildFact {
                    relative_components: child.relative_components.clone(),
                    node_kind: child.node_kind,
                    identity: child
                        .observed_identity
                        .clone()
                        .expect("success validates identity"),
                    content_fact: child
                        .content_fact
                        .clone()
                        .expect("success validates content"),
                })
                .collect();
            let handles = children
                .into_iter()
                .map(|child| VerifiedStagedNode {
                    relative_components: child.relative_components,
                    node_kind: child.node_kind,
                    identity: child.observed_identity.expect("success validates identity"),
                    content_fact: child.content_fact.expect("success validates content"),
                    handle: child.handle,
                })
                .collect();
            Ok((
                VerifiedStagedTree {
                    root_path: root.root_path,
                    root_identity: root.root_identity,
                    root_chain: root.root_chain,
                    root: root.root,
                    nodes,
                    handles,
                },
                copied_bytes,
            ))
        }
        Err(error) => Err(SpoolTreeFailure {
            error,
            retained: Box::new(retained),
        }),
    }
}

fn materialize_inner(
    spool: &mut std::fs::File,
    expected_spool: JournalSevenZipExtractIdentity,
    retained: &mut RetainedSpoolTree,
    cancelled: &AtomicBool,
    deadline: Instant,
    hooks: &mut impl MaterializeIo,
) -> Result<(), CopyError> {
    hooks.checkpoint(
        Checkpoint::BeforePreflight,
        cancelled,
        deadline,
        &retained.root.root,
        &retained.root.root_path,
    )?;
    if !retained.root.root_path.is_absolute()
        || directory_identity_text(&retained.root.root)? != retained.root.root_identity
        || !enumerate_directory_handle(&retained.root.root, 65_536, 8 * 1024 * 1024)?.is_empty()
    {
        return Err(CopyError::Unsupported);
    }
    let spool_identity = regular_file_identity(spool)?;
    if spool_identity.volume != expected_spool.volume
        || (u64::from(spool_identity.index_high) << 32 | u64::from(spool_identity.index_low))
            != expected_spool.file_index
        || spool.metadata().map_err(io)?.len() != retained.plan.total_size()
        || (spool_identity.volume.to_string()
            + ":"
            + &format!(
                "{:08x}{:08x}",
                spool_identity.index_high, spool_identity.index_low
            ))
            == retained.root.root_identity
    {
        return Err(CopyError::Unsupported);
    }

    let mut directories = HashMap::<Vec<String>, usize>::new();
    for (node_index, node) in retained.plan.nodes().iter().enumerate() {
        hooks.checkpoint(
            Checkpoint::BeforeChild(node_index),
            cancelled,
            deadline,
            &retained.root.root,
            &retained.root.root_path,
        )?;
        let components = node.components();
        let parent = if components.len() == 1 {
            &retained.root.root
        } else {
            let parent_index = *directories
                .get(&components[..components.len() - 1])
                .ok_or(CopyError::Unsupported)?;
            match &retained.children[parent_index].handle {
                PinnedSourceHandle::Directory(dir) => dir,
                PinnedSourceHandle::File(_) => return Err(CopyError::Unsupported),
            }
        };
        let leaf = components.last().ok_or(CopyError::Unsupported)?;
        let wide = leaf.encode_utf16().collect::<Vec<_>>();
        let is_dir = node.kind() == NodeKind::Directory;
        let raw = nt_create_child(parent, &wide, is_dir, true, FILE_SHARE_READ.0)?;
        // 作成直後に生handleをretainedへ移し、以後の失敗でも所有権を返せるようにする。
        let handle = if is_dir {
            PinnedSourceHandle::Directory(DirectoryLock(raw))
        } else {
            PinnedSourceHandle::File(unsafe { std::fs::File::from_raw_handle(raw.0) })
        };
        let mut relative_components = components
            .iter()
            .map(|part| part.encode_utf16().collect())
            .collect::<Vec<Vec<u16>>>();
        let node_kind = if is_dir {
            JournalDirectoryNodeKind::Directory
        } else {
            JournalDirectoryNodeKind::File
        };
        retained.children.push(RetainedStagedChild {
            node_index,
            handle,
            observed_identity: None,
            content_fact: None,
            copied_bytes: 0,
            relative_components: std::mem::take(&mut relative_components),
            node_kind,
        });
        let child = retained.children.last_mut().expect("just appended");
        child.observed_identity = Some(hooks.child_identity(node_index, &child.handle)?);
        if is_dir {
            // ディレクトリのfactはexact identityを確認した後だけ確定する。
            child.content_fact = Some("directory".into());
            directories.insert(components.to_vec(), retained.children.len() - 1);
            continue;
        }

        let range = node.spool_range().ok_or(CopyError::Unsupported)?;
        let length = range
            .end
            .checked_sub(range.start)
            .ok_or(CopyError::Unsupported)?;
        if Some(length) != node.declared_size() {
            return Err(CopyError::Unsupported);
        }
        spool.seek(SeekFrom::Start(range.start)).map_err(io)?;
        let mut remaining = length;
        let mut buffer = vec![0u8; CHUNK_BYTES];
        let mut hasher = Sha256::new();
        while remaining > 0 {
            native_checkpoint(cancelled, deadline)?;
            let wanted = usize::try_from(remaining.min(CHUNK_BYTES as u64))
                .map_err(|_| CopyError::Unsupported)?;
            let read = hooks
                .read_spool(spool, &mut buffer[..wanted])
                .map_err(super::io)?;
            if read == 0 {
                return Err(CopyError::Io("unexpected end of 7-Zip spool range".into()));
            }
            let mut offset = 0;
            while offset < read {
                hooks.checkpoint(
                    Checkpoint::BeforeWrite(node_index),
                    cancelled,
                    deadline,
                    &retained.root.root,
                    &retained.root.root_path,
                )?;
                let PinnedSourceHandle::File(out) = &mut child.handle else {
                    return Err(CopyError::Unsupported);
                };
                match hooks.write_child(node_index, out, &buffer[offset..read]) {
                    Ok(0) => return Err(CopyError::Io("zero-length staged write".into())),
                    Ok(written) => {
                        offset += written;
                        let amount = u64::try_from(written).map_err(|_| CopyError::Unsupported)?;
                        child.copied_bytes = child
                            .copied_bytes
                            .checked_add(amount)
                            .ok_or(CopyError::Unsupported)?;
                        retained.copied_bytes = retained
                            .copied_bytes
                            .checked_add(amount)
                            .ok_or(CopyError::Unsupported)?;
                        if retained.copied_bytes > retained.plan.total_size() {
                            return Err(CopyError::Unsupported);
                        }
                        hooks.checkpoint(
                            Checkpoint::AfterWrite(node_index),
                            cancelled,
                            deadline,
                            &retained.root.root,
                            &retained.root.root_path,
                        )?;
                    }
                    Err(error) => return Err(super::io(error)),
                }
            }
            hasher.update(&buffer[..read]);
            remaining -= u64::try_from(read).map_err(|_| CopyError::Unsupported)?;
        }
        let PinnedSourceHandle::File(out) = &mut child.handle else {
            return Err(CopyError::Unsupported);
        };
        if out.metadata().map_err(super::io)?.len() != length {
            return Err(CopyError::Unsupported);
        }
        hooks.sync_child(node_index, out).map_err(super::io)?;
        hooks.checkpoint(
            Checkpoint::AfterSync(node_index),
            cancelled,
            deadline,
            &retained.root.root,
            &retained.root.root_path,
        )?;
        child.content_fact = Some(format!("sha256:{:x}", hasher.finalize()));
    }
    hooks.checkpoint(
        Checkpoint::BeforeFinalSpoolValidation,
        cancelled,
        deadline,
        &retained.root.root,
        &retained.root.root_path,
    )?;
    let identity = regular_file_identity(spool)?;
    if identity.volume != expected_spool.volume
        || (u64::from(identity.index_high) << 32 | u64::from(identity.index_low))
            != expected_spool.file_index
        || spool.metadata().map_err(super::io)?.len() != retained.plan.total_size()
        || retained.copied_bytes != retained.plan.total_size()
    {
        return Err(CopyError::Unsupported);
    }
    let facts = retained
        .children
        .iter()
        .map(|child| {
            Ok(StagedChildFact {
                relative_components: child.relative_components.clone(),
                node_kind: child.node_kind,
                identity: child
                    .observed_identity
                    .clone()
                    .ok_or(CopyError::Unsupported)?,
                content_fact: child.content_fact.clone().ok_or(CopyError::Unsupported)?,
            })
        })
        .collect::<Result<Vec<_>, CopyError>>()?;
    hooks.checkpoint(
        Checkpoint::BeforeExactNamespaceValidation,
        cancelled,
        deadline,
        &retained.root.root,
        &retained.root.root_path,
    )?;
    verify_exact_published_children(&retained.root.root_path, &facts)?;
    hooks.checkpoint(
        Checkpoint::BeforeSuccess,
        cancelled,
        deadline,
        &retained.root.root,
        &retained.root.root_path,
    )
}

fn native_checkpoint(cancelled: &AtomicBool, deadline: Instant) -> Result<(), CopyError> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(CopyError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(CopyError::GuardRejected(
            "7-Zip staging time budget exhausted".into(),
        ));
    }
    Ok(())
}

fn file_identity_text(file: &std::fs::File) -> Result<String, CopyError> {
    let id = regular_file_identity(file)?;
    Ok(format!(
        "{}:{:08x}{:08x}",
        id.volume, id.index_high, id.index_low
    ))
}

fn io(error: impl std::fmt::Display) -> CopyError {
    CopyError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_provider::{IndexedRecord, Validated7zExtractPlan};
    use std::{fs, sync::atomic::AtomicBool};

    #[derive(Clone, Debug)]
    enum Fault {
        Identity(usize),
        Cancel(Checkpoint),
        Deadline(Checkpoint),
        ZeroWrite,
        ReadErrorAfter(usize),
        WriteErrorAfter(usize),
        UnexpectedEofAfterFirstRead,
        SyncFailure(usize),
        SpoolChange(Checkpoint),
        ForeignFile(Checkpoint, String),
        ForeignJunction(Checkpoint, String),
    }

    struct TestIo {
        fault: Option<Fault>,
        max_read: Option<usize>,
        max_write: Option<usize>,
        read_bytes: usize,
        write_bytes: usize,
        reads: usize,
        writes: usize,
        events: Vec<Checkpoint>,
        spool_alias: Option<std::fs::File>,
    }

    impl TestIo {
        fn new(fault: Option<Fault>) -> Self {
            Self {
                fault,
                max_read: None,
                max_write: None,
                read_bytes: 0,
                write_bytes: 0,
                reads: 0,
                writes: 0,
                events: Vec::new(),
                spool_alias: None,
            }
        }
    }

    impl MaterializeIo for TestIo {
        fn child_identity(
            &mut self,
            index: usize,
            child: &PinnedSourceHandle,
        ) -> Result<String, CopyError> {
            if self
                .fault
                .as_ref()
                .is_some_and(|fault| matches!(fault, Fault::Identity(at) if *at == index))
            {
                return Err(CopyError::Io(
                    "injected child identity query failure".into(),
                ));
            }
            match child {
                PinnedSourceHandle::Directory(dir) => directory_identity_text(dir),
                PinnedSourceHandle::File(file) => file_identity_text(file),
            }
        }
        fn read_spool(
            &mut self,
            spool: &mut std::fs::File,
            buffer: &mut [u8],
        ) -> std::io::Result<usize> {
            if matches!(self.fault, Some(Fault::ReadErrorAfter(limit)) if self.read_bytes >= limit)
            {
                return Err(std::io::Error::other("injected spool read error"));
            }
            if matches!(self.fault, Some(Fault::ReadErrorAfter(limit)) if self.read_bytes < limit) {
                let limit = match self.fault {
                    Some(Fault::ReadErrorAfter(n)) => n - self.read_bytes,
                    _ => 0,
                };
                let size = buffer.len().min(limit);
                let read = spool.read(&mut buffer[..size])?;
                self.read_bytes += read;
                self.reads += 1;
                return Ok(read);
            }
            if matches!(self.fault, Some(Fault::UnexpectedEofAfterFirstRead)) && self.reads == 0 {
                if let Some(alias) = &self.spool_alias {
                    alias.set_len(1)?;
                }
            }
            let size = self.max_read.map_or(buffer.len(), |n| buffer.len().min(n));
            let read = spool.read(&mut buffer[..size])?;
            self.read_bytes += read;
            self.reads += 1;
            Ok(read)
        }
        fn write_child(
            &mut self,
            _index: usize,
            file: &mut std::fs::File,
            bytes: &[u8],
        ) -> std::io::Result<usize> {
            if matches!(self.fault, Some(Fault::ZeroWrite)) {
                return Ok(0);
            }
            if matches!(self.fault, Some(Fault::WriteErrorAfter(limit)) if self.write_bytes >= limit)
            {
                return Err(std::io::Error::other("injected staged write error"));
            }
            let size = self.max_write.map_or(bytes.len(), |n| bytes.len().min(n));
            let size = match self.fault {
                Some(Fault::WriteErrorAfter(limit)) => {
                    size.min(limit.saturating_sub(self.write_bytes))
                }
                _ => size,
            };
            let written = file.write(&bytes[..size])?;
            self.write_bytes += written;
            self.writes += 1;
            Ok(written)
        }
        fn sync_child(&mut self, index: usize, file: &std::fs::File) -> std::io::Result<()> {
            if matches!(self.fault, Some(Fault::SyncFailure(at)) if at == index) {
                return Err(std::io::Error::other("injected staged sync failure"));
            }
            file.sync_all()
        }
        fn checkpoint(
            &mut self,
            point: Checkpoint,
            cancelled: &AtomicBool,
            deadline: Instant,
            root: &DirectoryLock,
            root_path: &Path,
        ) -> Result<(), CopyError> {
            self.events.push(point);
            if matches!(self.fault, Some(Fault::Cancel(at)) if at == point) {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            }
            if matches!(self.fault, Some(Fault::Deadline(at)) if at == point) {
                return native_checkpoint(
                    cancelled,
                    Instant::now() - std::time::Duration::from_secs(1),
                );
            }
            if matches!(self.fault, Some(Fault::SpoolChange(at)) if at == point) {
                if let Some(alias) = &self.spool_alias {
                    alias.set_len(0).map_err(io)?;
                }
            }
            if let Some(Fault::ForeignFile(at, name)) = &self.fault {
                if *at == point {
                    let raw = nt_create_child(
                        root,
                        &name.encode_utf16().collect::<Vec<_>>(),
                        false,
                        true,
                        FILE_SHARE_READ.0,
                    )?;
                    let mut file = unsafe { std::fs::File::from_raw_handle(raw.0) };
                    file.write_all(b"foreign sentinel").map_err(io)?;
                    file.sync_all().map_err(io)?;
                }
            }
            if let Some(Fault::ForeignJunction(at, name)) = &self.fault {
                if *at == point {
                    let target = root_path
                        .parent()
                        .expect("stage parent")
                        .join("junction-target");
                    fs::create_dir_all(&target).map_err(io)?;
                    fs::write(target.join("sentinel"), b"target sentinel").map_err(io)?;
                    crate::operations::privileged::create_junction_direct(
                        &target,
                        &root_path.join(name),
                    )
                    .map_err(io)?;
                }
            }
            native_checkpoint(cancelled, deadline)
        }
    }

    struct TestParent(std::path::PathBuf);

    impl std::ops::Deref for TestParent {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for TestParent {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestParent {
        fn drop(&mut self) {
            // panic経路でもテスト用treeを残さない。失敗だけstderrへ出す。
            if let Err(error) = fs::remove_dir_all(&self.0)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "failed to remove spool-tree test fixture {}: {error}",
                    self.0.display()
                );
            }
        }
    }

    fn test_parent() -> TestParent {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "gpui-filer-7z-spool-tree-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create test parent");
        TestParent(path)
    }

    fn write_spool(parent: &Path, bytes: &[u8]) -> std::fs::File {
        static NEXT_SPOOL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        use std::os::windows::fs::OpenOptionsExt;
        let path = parent.join(format!(
            "payload-{}.spool",
            NEXT_SPOOL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(path)
            .expect("create spool");
        file.write_all(bytes).expect("write spool fixture");
        file.sync_all().expect("sync spool fixture");
        file
    }

    fn spool_identity(file: &std::fs::File) -> JournalSevenZipExtractIdentity {
        let id = regular_file_identity(file).expect("spool identity");
        JournalSevenZipExtractIdentity {
            volume: id.volume,
            file_index: (u64::from(id.index_high) << 32) | u64::from(id.index_low),
        }
    }

    fn file_plan(name: &str, size: usize) -> Arc<Validated7zExtractPlan> {
        Arc::new(
            Validated7zExtractPlan::validate(vec![IndexedRecord {
                index: 0,
                components: vec![name.into()],
                kind: NodeKind::File,
                declared_size: Some(size as u64),
                crc32: None,
            }])
            .expect("file plan"),
        )
    }

    fn create_foreign(root: &DirectoryLock, name: &str, bytes: &[u8]) {
        let raw = nt_create_child(
            root,
            &name.encode_utf16().collect::<Vec<_>>(),
            false,
            true,
            FILE_SHARE_READ.0,
        )
        .expect("foreign create");
        let mut file = unsafe { std::fs::File::from_raw_handle(raw.0) };
        file.write_all(bytes).expect("foreign bytes");
        file.sync_all().expect("foreign sync");
    }

    fn expect_failure(
        spool: &mut std::fs::File,
        identity: JournalSevenZipExtractIdentity,
        plan: Arc<Validated7zExtractPlan>,
        root: StagingRootLease,
        cancelled: &AtomicBool,
        hooks: &mut TestIo,
    ) -> SpoolTreeFailure {
        match materialize_with_io(
            spool,
            identity,
            plan,
            root,
            cancelled,
            Instant::now() + std::time::Duration::from_secs(60),
            hooks,
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("injected failure unexpectedly succeeded"),
        }
    }

    #[test]
    fn indexed_ranges_materialize_to_name_tree_without_changing_spool() {
        let parent = test_parent();
        let payload_z = b"z-order-first".repeat(6_000);
        let payload_a = b"nested-second".to_vec();
        let payload_at = b"at-leading".to_vec();
        let mut spool_bytes = payload_z.clone();
        spool_bytes.extend_from_slice(&payload_a);
        spool_bytes.extend_from_slice(&payload_at);
        let mut spool = write_spool(&parent, &spool_bytes);
        let expected_spool = spool_identity(&spool);
        let plan = Arc::new(
            Validated7zExtractPlan::validate(vec![
                IndexedRecord {
                    index: 0,
                    components: vec!["- 日本 @".into(), " @child .txt".into()],
                    kind: NodeKind::File,
                    declared_size: Some(payload_z.len() as u64),
                    crc32: None,
                },
                IndexedRecord {
                    index: 1,
                    components: vec!["a".into(), "child.txt".into()],
                    kind: NodeKind::File,
                    declared_size: Some(payload_a.len() as u64),
                    crc32: None,
                },
                IndexedRecord {
                    index: 2,
                    components: vec!["zero.txt".into()],
                    kind: NodeKind::File,
                    declared_size: Some(0),
                    crc32: None,
                },
                IndexedRecord {
                    index: 3,
                    components: vec!["empty".into()],
                    kind: NodeKind::Directory,
                    declared_size: Some(0),
                    crc32: None,
                },
                IndexedRecord {
                    index: 4,
                    components: vec!["@lead file.txt".into()],
                    kind: NodeKind::File,
                    declared_size: Some(payload_at.len() as u64),
                    crc32: None,
                },
            ])
            .expect("validated plan"),
        );
        let root = match super::retained_tree::create_root(&parent.join("stage")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("create stage: {other:?}"),
        };
        let cancelled = AtomicBool::new(false);
        let mut hooks = TestIo::new(None);
        let (tree, copied) = materialize_with_io(
            &mut spool,
            expected_spool,
            plan,
            root,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(30),
            &mut hooks,
        )
        .expect("materialize validated ranges");
        assert_eq!(copied, spool_bytes.len() as u64);
        assert_eq!(
            fs::read(parent.join("stage/- 日本 @/ @child .txt")).expect("unicode output"),
            payload_z
        );
        assert_eq!(
            fs::read(parent.join("stage/a/child.txt")).expect("a output"),
            payload_a
        );
        assert_eq!(
            fs::read(parent.join("stage/@lead file.txt")).expect("at output"),
            payload_at
        );
        assert!(parent.join("stage/empty").is_dir());
        assert_eq!(tree.nodes.len(), 7);
        assert_eq!(
            fs::read(parent.join("stage/zero.txt")).expect("empty file"),
            b""
        );
        assert!(tree.nodes.iter().any(|node| {
            node.relative_components.last() == Some(&"zero.txt".encode_utf16().collect::<Vec<_>>())
                && node.content_fact
                    == "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        }));
        let expected_z_fact = format!("sha256:{:x}", Sha256::digest(&payload_z));
        let expected_a_fact = format!("sha256:{:x}", Sha256::digest(&payload_a));
        let expected_at_fact = format!("sha256:{:x}", Sha256::digest(&payload_at));
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.content_fact == expected_z_fact)
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.content_fact == expected_a_fact)
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.content_fact == expected_at_fact)
        );
        let mut original = spool.try_clone().expect("spool observation handle");
        original
            .seek(SeekFrom::Start(0))
            .expect("reset source observation");
        let mut actual_spool = Vec::new();
        original
            .read_to_end(&mut actual_spool)
            .expect("read original spool");
        assert_eq!(actual_spool, spool_bytes);
        drop(original);
        let published = super::retained_tree::publish(tree, &parent.join("published"), &cancelled)
            .expect("publish verified staged tree");
        assert_eq!(
            fs::read(parent.join("published/- 日本 @/ @child .txt")).expect("published unicode"),
            payload_z
        );
        assert_eq!(
            fs::read(parent.join("published/a/child.txt")).expect("published nested"),
            payload_a
        );
        assert_eq!(
            fs::read(parent.join("published/@lead file.txt")).expect("published at"),
            payload_at
        );
        let (_, pins) = super::verified_published_identities_for_test(&published)
            .expect("published identities");
        assert_eq!(pins.len(), 7);
        drop(published);
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn empty_plan_returns_an_empty_verified_stage() {
        let parent = test_parent();
        let mut spool = write_spool(&parent, &[]);
        let expected_spool = spool_identity(&spool);
        let plan = Arc::new(Validated7zExtractPlan::validate(Vec::new()).expect("empty plan"));
        let root = match super::retained_tree::create_root(&parent.join("stage")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("create stage: {other:?}"),
        };
        let (tree, copied) = materialize(
            &mut spool,
            expected_spool,
            plan,
            root,
            &AtomicBool::new(false),
            Instant::now() + std::time::Duration::from_secs(30),
        )
        .expect("empty stage");
        assert_eq!(copied, 0);
        assert!(tree.nodes.is_empty());
        drop(tree);
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn entry_cancel_and_wrong_spool_identity_retain_empty_stage() {
        let parent = test_parent();
        let mut spool = write_spool(&parent, b"x");
        let plan = Arc::new(
            Validated7zExtractPlan::validate(vec![IndexedRecord {
                index: 0,
                components: vec!["payload.bin".into()],
                kind: NodeKind::File,
                declared_size: Some(1),
                crc32: None,
            }])
            .expect("plan"),
        );
        let cancelled = AtomicBool::new(true);
        let root = match super::retained_tree::create_root(&parent.join("cancel-stage")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("create stage: {other:?}"),
        };
        let identity = spool_identity(&spool);
        let failure = match materialize(
            &mut spool,
            identity.clone(),
            plan.clone(),
            root,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(30),
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("entry cancellation unexpectedly materialized a tree"),
        };
        assert!(matches!(failure.error, CopyError::Cancelled));
        assert!(failure.retained.children.is_empty());
        assert!(
            fs::read_dir(parent.join("cancel-stage"))
                .expect("stage enumeration")
                .next()
                .is_none()
        );
        drop(failure);

        let root = match super::retained_tree::create_root(&parent.join("identity-stage")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("create stage: {other:?}"),
        };
        let failure = match materialize(
            &mut spool,
            JournalSevenZipExtractIdentity {
                volume: identity.volume.wrapping_add(1),
                file_index: identity.file_index,
            },
            plan,
            root,
            &AtomicBool::new(false),
            Instant::now() + std::time::Duration::from_secs(30),
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("wrong spool identity unexpectedly materialized a tree"),
        };
        assert!(failure.retained.children.is_empty());
        assert_eq!(failure.retained.copied_bytes, 0);
        assert!(
            fs::read_dir(parent.join("identity-stage"))
                .expect("stage enumeration")
                .next()
                .is_none()
        );
        drop(failure);
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn preflight_rejects_foreign_root_wrong_root_and_short_or_excess_spools() {
        let parent = test_parent();
        let payload = b"xy";
        let plan = file_plan("x.bin", payload.len());
        let mut spool = write_spool(&parent, payload);
        let identity = spool_identity(&spool);

        let root = match super::retained_tree::create_root(&parent.join("foreign-root")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        create_foreign(&root.root, "foreign.txt", b"keep me");
        let mut hooks = TestIo::new(None);
        let failure = expect_failure(
            &mut spool,
            identity.clone(),
            plan.clone(),
            root,
            &AtomicBool::new(false),
            &mut hooks,
        );
        assert!(failure.retained.children.is_empty());
        assert_eq!(
            fs::read(parent.join("foreign-root/foreign.txt")).expect("foreign sentinel"),
            b"keep me"
        );
        drop(failure);

        let mut root = match super::retained_tree::create_root(&parent.join("wrong-root")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        root.root_identity.push_str("-wrong");
        let failure = expect_failure(
            &mut spool,
            identity.clone(),
            plan.clone(),
            root,
            &AtomicBool::new(false),
            &mut hooks,
        );
        assert!(failure.retained.children.is_empty());
        drop(failure);

        for (label, bytes) in [("short", &b"x"[..]), ("excess", &b"xyz"[..])] {
            let mut wrong_size_spool = write_spool(&parent, bytes);
            let actual_identity = spool_identity(&wrong_size_spool);
            let root = match super::retained_tree::create_root(&parent.join(label)) {
                super::retained_tree::CreateRoot::Ready(root) => root,
                other => panic!("{other:?}"),
            };
            let failure = expect_failure(
                &mut wrong_size_spool,
                actual_identity,
                plan.clone(),
                root,
                &AtomicBool::new(false),
                &mut hooks,
            );
            assert!(failure.retained.children.is_empty());
            assert_eq!(failure.retained.copied_bytes, 0);
            drop(failure);
            drop(wrong_size_spool);
        }
        let root = match super::retained_tree::create_root(&parent.join("expired")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        let failure = match materialize(
            &mut spool,
            identity,
            plan.clone(),
            root,
            &AtomicBool::new(false),
            Instant::now() - std::time::Duration::from_secs(1),
        ) {
            Err(failure) => failure,
            Ok(_) => panic!("expired entry deadline unexpectedly materialized a tree"),
        };
        assert!(failure.retained.children.is_empty());
        assert!(Arc::ptr_eq(&failure.retained.plan, &plan));
        assert_eq!(failure.retained.copied_bytes, 0);
        assert!(matches!(failure.error, CopyError::GuardRejected(_)));
        drop(failure);
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn post_create_identity_failures_keep_file_and_directory_handles() {
        let parent = test_parent();
        let records = vec![
            IndexedRecord {
                index: 0,
                components: vec!["a.bin".into()],
                kind: NodeKind::File,
                declared_size: Some(1),
                crc32: None,
            },
            IndexedRecord {
                index: 1,
                components: vec!["b".into()],
                kind: NodeKind::Directory,
                declared_size: Some(0),
                crc32: None,
            },
            IndexedRecord {
                index: 2,
                components: vec!["b".into(), "c.bin".into()],
                kind: NodeKind::File,
                declared_size: Some(1),
                crc32: None,
            },
        ];
        let plan = Arc::new(Validated7zExtractPlan::validate(records).expect("plan"));
        let mut spool = write_spool(&parent, b"ac");
        let identity = spool_identity(&spool);
        for (index, expected_path, is_directory) in
            [(1usize, "b", true), (2usize, "b/c.bin", false)]
        {
            let root =
                match super::retained_tree::create_root(&parent.join(format!("stage-{index}"))) {
                    super::retained_tree::CreateRoot::Ready(root) => root,
                    other => panic!("{other:?}"),
                };
            let mut hooks = TestIo::new(Some(Fault::Identity(index)));
            let failure = expect_failure(
                &mut spool,
                identity.clone(),
                plan.clone(),
                root,
                &AtomicBool::new(false),
                &mut hooks,
            );
            assert_eq!(failure.retained.children.len(), index + 1);
            let current = &failure.retained.children[index];
            assert!(current.observed_identity.is_none());
            assert!(current.content_fact.is_none());
            assert_eq!(
                current
                    .relative_components
                    .iter()
                    .map(|part| String::from_utf16_lossy(part))
                    .collect::<Vec<_>>()
                    .join("/"),
                expected_path
            );
            assert_eq!(
                matches!(current.handle, PinnedSourceHandle::Directory(_)),
                is_directory
            );
            match &current.handle {
                PinnedSourceHandle::Directory(directory) => assert!(
                    !directory_identity_text(directory)
                        .expect("held created directory identity")
                        .is_empty()
                ),
                PinnedSourceHandle::File(file) => assert!(
                    !file_identity_text(file)
                        .expect("held created file identity")
                        .is_empty()
                ),
            }
            if !is_directory {
                assert!(
                    fs::write(parent.join(format!("stage-{index}/b/c.bin")), b"blocked").is_err()
                );
            }
            if failure
                .retained
                .children
                .iter()
                .any(|child| child.node_kind == JournalDirectoryNodeKind::Directory)
            {
                assert!(
                    fs::rename(
                        parent.join(format!("stage-{index}/b")),
                        parent.join(format!("stage-{index}/renamed"))
                    )
                    .is_err()
                );
            }
            let persisted = failure.retained.root.root_path.clone();
            drop(failure);
            assert!(
                persisted.exists(),
                "dropping retained state must not delete stage"
            );
            assert!(
                persisted.join(expected_path).exists(),
                "dropping retained state must leave created child"
            );
        }
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn cancellation_and_deadline_after_copy_bound_all_later_writes() {
        let parent = test_parent();
        let bytes = vec![b'q'; CHUNK_BYTES * 2 + 31];
        let plan = file_plan("a.bin", bytes.len());
        for (label, fault) in [
            ("chunk", Fault::Cancel(Checkpoint::AfterWrite(0))),
            ("sync", Fault::Cancel(Checkpoint::AfterSync(0))),
            ("deadline", Fault::Deadline(Checkpoint::AfterWrite(0))),
        ] {
            let mut spool = write_spool(&parent, &bytes);
            let identity = spool_identity(&spool);
            let root = match super::retained_tree::create_root(&parent.join(label)) {
                super::retained_tree::CreateRoot::Ready(root) => root,
                other => panic!("{other:?}"),
            };
            let mut hooks = TestIo::new(Some(fault));
            let cancelled = AtomicBool::new(false);
            let failure = expect_failure(
                &mut spool,
                identity,
                plan.clone(),
                root,
                &cancelled,
                &mut hooks,
            );
            if label == "deadline" {
                assert!(matches!(failure.error, CopyError::GuardRejected(_)));
            } else {
                assert!(matches!(failure.error, CopyError::Cancelled));
            }
            assert_eq!(failure.retained.copied_bytes as usize, hooks.write_bytes);
            assert!(failure.retained.children[0].content_fact.is_none());
            if label == "deadline" {
                assert_eq!(hooks.write_bytes, CHUNK_BYTES);
                assert_eq!(
                    failure.retained.children[0].copied_bytes as usize,
                    CHUNK_BYTES
                );
                assert_eq!(
                    fs::read(parent.join("deadline/a.bin")).unwrap(),
                    bytes[..CHUNK_BYTES]
                );
            }
            if label != "sync" {
                assert_eq!(
                    hooks.writes, 1,
                    "no writes after observed cancellation/deadline"
                );
            }
            drop(failure);
            drop(spool);
        }

        let mut spool = write_spool(&parent, b"ab");
        let identity = spool_identity(&spool);
        let multi = Arc::new(
            Validated7zExtractPlan::validate(vec![
                IndexedRecord {
                    index: 0,
                    components: vec!["a".into()],
                    kind: NodeKind::File,
                    declared_size: Some(1),
                    crc32: None,
                },
                IndexedRecord {
                    index: 1,
                    components: vec!["b".into()],
                    kind: NodeKind::File,
                    declared_size: Some(1),
                    crc32: None,
                },
            ])
            .expect("two files"),
        );
        let root = match super::retained_tree::create_root(&parent.join("between")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        let mut hooks = TestIo::new(Some(Fault::Cancel(Checkpoint::BeforeChild(1))));
        let failure = expect_failure(
            &mut spool,
            identity,
            multi,
            root,
            &AtomicBool::new(false),
            &mut hooks,
        );
        assert_eq!(failure.retained.children.len(), 1);
        assert!(failure.retained.children[0].content_fact.is_some());
        assert_eq!(hooks.writes, 1);
        drop(failure);
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn short_io_and_failures_retain_confirmed_prefixes() {
        let parent = test_parent();
        let bytes = b"0123456789abcdef";
        let plan = file_plan("x.bin", bytes.len());
        let mut spool = write_spool(&parent, bytes);
        let identity = spool_identity(&spool);
        let root = match super::retained_tree::create_root(&parent.join("short")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        let mut hooks = TestIo::new(None);
        hooks.max_read = Some(3);
        hooks.max_write = Some(2);
        let (tree, copied) = materialize_with_io(
            &mut spool,
            identity,
            plan.clone(),
            root,
            &AtomicBool::new(false),
            Instant::now() + std::time::Duration::from_secs(60),
            &mut hooks,
        )
        .expect("short reads/writes complete");
        assert_eq!(copied, bytes.len() as u64);
        assert_eq!(fs::read(parent.join("short/x.bin")).unwrap(), bytes);
        drop(tree);
        drop(spool);

        for (label, fault, prefix) in [
            ("zero", Fault::ZeroWrite, 0usize),
            ("read-error", Fault::ReadErrorAfter(4), 4usize),
            ("partial-write-error", Fault::WriteErrorAfter(4), 4usize),
            ("sync-error", Fault::SyncFailure(0), bytes.len()),
            ("unexpected-eof", Fault::UnexpectedEofAfterFirstRead, 1usize),
        ] {
            let mut spool = write_spool(&parent, bytes);
            let identity = spool_identity(&spool);
            let root = match super::retained_tree::create_root(&parent.join(label)) {
                super::retained_tree::CreateRoot::Ready(root) => root,
                other => panic!("{other:?}"),
            };
            let mut hooks = TestIo::new(Some(fault));
            if label == "unexpected-eof" {
                hooks.spool_alias = Some(spool.try_clone().expect("mutation alias"));
            }
            let failure = expect_failure(
                &mut spool,
                identity,
                plan.clone(),
                root,
                &AtomicBool::new(false),
                &mut hooks,
            );
            assert_eq!(failure.retained.copied_bytes as usize, prefix, "{label}");
            assert_eq!(
                failure.retained.children[0].copied_bytes as usize, prefix,
                "{label}"
            );
            assert!(
                failure.retained.children[0].content_fact.is_none(),
                "{label}"
            );
            assert_eq!(
                fs::read(parent.join(format!("{label}/x.bin"))).unwrap_or_default(),
                bytes[..prefix],
                "{label}"
            );
            drop(failure);
            drop(spool);
        }
        fs::remove_dir_all(parent).expect("remove test parent");
    }

    #[test]
    fn collision_and_final_validation_failures_preserve_foreign_and_owned_data() {
        let parent = test_parent();
        let payload = b"owned";
        let plan = Arc::new(
            Validated7zExtractPlan::validate(vec![
                IndexedRecord {
                    index: 0,
                    components: vec!["a".into()],
                    kind: NodeKind::File,
                    declared_size: Some(5),
                    crc32: None,
                },
                IndexedRecord {
                    index: 1,
                    components: vec!["b".into()],
                    kind: NodeKind::File,
                    declared_size: Some(5),
                    crc32: None,
                },
            ])
            .expect("two files"),
        );
        let mut spool = write_spool(&parent, b"ownedowned");
        let identity = spool_identity(&spool);
        let root = match super::retained_tree::create_root(&parent.join("collision")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        let mut hooks = TestIo::new(Some(Fault::ForeignFile(
            Checkpoint::BeforeChild(1),
            "b".into(),
        )));
        let failure = expect_failure(
            &mut spool,
            identity.clone(),
            plan.clone(),
            root,
            &AtomicBool::new(false),
            &mut hooks,
        );
        assert_eq!(failure.retained.children.len(), 1);
        assert_eq!(
            fs::read(parent.join("collision/b")).unwrap(),
            b"foreign sentinel"
        );
        assert_eq!(fs::read(parent.join("collision/a")).unwrap(), payload);
        drop(failure);

        let root = match super::retained_tree::create_root(&parent.join("junction-collision")) {
            super::retained_tree::CreateRoot::Ready(root) => root,
            other => panic!("{other:?}"),
        };
        let mut hooks = TestIo::new(Some(Fault::ForeignJunction(
            Checkpoint::BeforeChild(1),
            "b".into(),
        )));
        let failure = expect_failure(
            &mut spool,
            identity.clone(),
            plan.clone(),
            root,
            &AtomicBool::new(false),
            &mut hooks,
        );
        assert_eq!(failure.retained.children.len(), 1);
        assert_eq!(
            fs::read(parent.join("junction-target/sentinel")).unwrap(),
            b"target sentinel"
        );
        assert!(
            fs::symlink_metadata(parent.join("junction-collision/b"))
                .expect("junction object")
                .file_type()
                .is_symlink()
        );
        drop(failure);

        for (label, fault) in [
            (
                "spool-change",
                Fault::SpoolChange(Checkpoint::BeforeFinalSpoolValidation),
            ),
            (
                "namespace-change",
                Fault::ForeignFile(Checkpoint::BeforeExactNamespaceValidation, "foreign".into()),
            ),
        ] {
            let mut spool = write_spool(&parent, b"ownedowned");
            let identity = spool_identity(&spool);
            let alias = spool.try_clone().expect("spool alias");
            let root = match super::retained_tree::create_root(&parent.join(label)) {
                super::retained_tree::CreateRoot::Ready(root) => root,
                other => panic!("{other:?}"),
            };
            let mut hooks = TestIo::new(Some(fault));
            hooks.spool_alias = Some(alias);
            let failure = expect_failure(
                &mut spool,
                identity,
                plan.clone(),
                root,
                &AtomicBool::new(false),
                &mut hooks,
            );
            assert_eq!(failure.retained.children.len(), 2);
            assert!(hooks.events.contains(&Checkpoint::AfterSync(0)));
            assert!(hooks.events.contains(&Checkpoint::AfterSync(1)));
            assert!(
                hooks
                    .events
                    .contains(&Checkpoint::BeforeFinalSpoolValidation)
            );
            if label == "namespace-change" {
                assert!(
                    hooks
                        .events
                        .contains(&Checkpoint::BeforeExactNamespaceValidation)
                );
                assert_eq!(
                    fs::read(parent.join("namespace-change/foreign")).unwrap(),
                    b"foreign sentinel"
                );
            }
            assert!(fs::read(parent.join(format!("{label}/a"))).is_ok());
            drop(failure);
            drop(spool);
        }
        drop(spool);
        fs::remove_dir_all(parent).expect("remove test parent");
    }
}
