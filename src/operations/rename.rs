//! Windows rename capability gate.
//!
//! This adapter deliberately accepts one parent-fixed name component only.  A
//! future inline editor submits a detached request here; it must not use a
//! display row or turn a rename into a shell command line.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameError {
    Unsupported,
    IdentityMismatch,
    ReparsePoint,
    CrossVolume,
    DestinationExists,
    Cancelled,
    InvalidName,
    Partial(String),
    MutationUncertain(String),
    Io(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameReceipt {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[cfg(windows)]
pub(crate) struct DestinationIdentityLease {
    _inner: windows::DestinationIdentityLease,
}

#[cfg(windows)]
pub(crate) fn pin_destination_directory_identity(
    path: &Path,
    expected_identity: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<DestinationIdentityLease, RenameError> {
    windows::pin_destination_directory_identity(path, expected_identity)
        .map(|lease| DestinationIdentityLease { _inner: lease })
}

#[cfg(windows)]
pub(crate) fn rename_held_file_no_replace(
    source: &std::fs::File,
    destination: &Path,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<(), RenameError> {
    windows::rename_held_file_no_replace(source, destination, expected_destination_directory)
}

#[cfg(windows)]
pub(crate) fn rename_relative_handle_no_replace(
    source: *mut std::ffi::c_void,
    destination_parent: *mut std::ffi::c_void,
    name: &std::ffi::OsStr,
) -> Result<(), RenameError> {
    windows::rename_relative_handle_no_replace(source, destination_parent, name)
}

pub(crate) fn rename_sibling_guarded(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), RenameError> {
    let parent = destination.parent().ok_or(RenameError::InvalidName)?;
    if source.parent() != Some(parent) {
        return Err(RenameError::IdentityMismatch);
    }
    #[cfg(windows)]
    {
        windows::rename_guarded(
            source,
            destination,
            expected_source,
            expected_parent,
            cancelled,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (
            source,
            destination,
            expected_source,
            expected_parent,
            cancelled,
        );
        Err(RenameError::Unsupported)
    }
}

#[cfg(windows)]
pub(crate) fn probe_reparse_source(
    source: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_parent: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<crate::operations::model::RenameReparseGuard, RenameError> {
    windows::probe_reparse_source(source, expected_source, expected_parent)
}

#[cfg(not(windows))]
pub(crate) fn probe_reparse_source(
    _source: &Path,
    _expected_source: &crate::filesystem_identity::FilesystemIdentity,
    _expected_parent: &crate::filesystem_identity::FilesystemIdentity,
) -> Result<crate::operations::model::RenameReparseGuard, RenameError> {
    Err(RenameError::Unsupported)
}

#[cfg(windows)]
pub(crate) fn rename_reparse_sibling_guarded(
    source: &Path,
    destination: &Path,
    guard: &crate::operations::model::RenameReparseGuard,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), RenameError> {
    rename_reparse_sibling_guarded_for_kind(source, destination, guard, None, cancelled)
}

#[cfg(windows)]
pub(crate) fn rename_reparse_sibling_guarded_for_kind(
    source: &Path,
    destination: &Path,
    guard: &crate::operations::model::RenameReparseGuard,
    expected_kind: Option<crate::directory::EntryKind>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), RenameError> {
    if is_case_only_rename(
        source,
        &destination
            .file_name()
            .ok_or(RenameError::InvalidName)?
            .to_string_lossy(),
    ) {
        return Err(RenameError::ReparsePoint);
    }
    windows::rename_reparse_guarded(source, destination, guard, expected_kind, cancelled)
}

#[cfg(not(windows))]
pub(crate) fn rename_reparse_sibling_guarded(
    _source: &Path,
    _destination: &Path,
    _guard: &crate::operations::model::RenameReparseGuard,
    _cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), RenameError> {
    Err(RenameError::Unsupported)
}

#[cfg(not(windows))]
pub(crate) fn rename_reparse_sibling_guarded_for_kind(
    _source: &Path,
    _destination: &Path,
    _guard: &crate::operations::model::RenameReparseGuard,
    _expected_kind: Option<crate::directory::EntryKind>,
    _cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), RenameError> {
    Err(RenameError::Unsupported)
}

#[cfg(windows)]
pub(crate) fn rename_case_only_guarded(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    reparse_guard: Option<&crate::operations::model::RenameReparseGuard>,
    expected_kind: Option<crate::directory::EntryKind>,
    token: super::model::AttemptToken,
    item: super::model::ItemId,
    cancelled: &std::sync::atomic::AtomicBool,
    controls: &std::sync::mpsc::Receiver<super::executor::WorkerControl>,
    emit: &mut impl FnMut(super::executor::ExecutorFact),
) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
    let reparse_tag = reparse_guard.map(|guard| guard.reparse_tag);
    if let Some(guard) = reparse_guard {
        if guard.source_identity != *expected_source || guard.parent_identity != *expected_parent {
            return Err(RenameError::IdentityMismatch);
        }
    }
    windows::rename_case_only_guarded(
        source,
        destination,
        expected_source,
        expected_parent,
        reparse_tag,
        expected_kind,
        token,
        item,
        cancelled,
        controls,
        emit,
    )
}

#[cfg(not(windows))]
pub(crate) fn rename_case_only_guarded(
    _source: &Path,
    _destination: &Path,
    _expected_source: &crate::filesystem_identity::FilesystemIdentity,
    _expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    _reparse_guard: Option<&crate::operations::model::RenameReparseGuard>,
    _expected_kind: Option<crate::directory::EntryKind>,
    _token: super::model::AttemptToken,
    _item: super::model::ItemId,
    _cancelled: &std::sync::atomic::AtomicBool,
    _controls: &std::sync::mpsc::Receiver<super::executor::WorkerControl>,
    _emit: &mut impl FnMut(super::executor::ExecutorFact),
) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
    Err(RenameError::Unsupported)
}

/// Same-volume move is a handle-bound rename, never copy-then-delete.  A
/// cross-volume request is rejected by `FileRenameInfo`; a later adapter may
/// add copy/identity/recheck/delete as a distinct capability.
pub fn move_same_volume_no_replace(
    source: &Path,
    destination: &Path,
) -> Result<Option<RenameReceipt>, RenameError> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(RenameError::InvalidName)?;
    if !is_single_name(name) || destination.parent().is_none() {
        return Err(RenameError::InvalidName);
    }
    if source == destination {
        return Ok(None);
    }
    #[cfg(target_os = "windows")]
    {
        windows::validate_same_volume_move(source, destination)?;
        windows::rename(source, destination).map(|()| {
            Some(RenameReceipt {
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
            })
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(RenameError::Unsupported)
    }
}

/// Guarded internal moves bind both the source object and destination
/// directory to request-time identities. This path never enables a
/// cross-volume or broker fallback.
pub fn move_same_volume_no_replace_guarded(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<RenameReceipt>, RenameError> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(RenameError::InvalidName)?;
    if !is_single_name(name) || destination.parent().is_none() {
        return Err(RenameError::InvalidName);
    }
    if source == destination {
        return Ok(None);
    }
    #[cfg(target_os = "windows")]
    {
        windows::rename_guarded(
            source,
            destination,
            expected_source,
            expected_destination_directory,
            cancelled,
        )
        .map(|()| {
            Some(RenameReceipt {
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
            })
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (expected_source, expected_destination_directory, cancelled);
        Err(RenameError::Unsupported)
    }
}

pub(crate) fn move_same_volume_no_replace_guarded_with_undo(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
    #[cfg(windows)]
    { windows::move_same_volume_no_replace_guarded_with_undo(source, destination, expected_source, expected_destination_directory, cancelled, |_| {}) }
    #[cfg(not(windows))]
    { let _ = (source, destination, expected_source, expected_destination_directory, cancelled); Ok(None) }
}

/// Clipboard Move needs to distinguish a proven cross-volume route from an
/// incomplete same-volume probe before selecting its own identity-bound worker.
pub(crate) fn move_same_volume_no_replace_clipboard_with_undo(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
    #[cfg(windows)]
    { windows::move_same_volume_no_replace_clipboard_with_undo(source, destination, expected_source, expected_destination_directory, cancelled) }
    #[cfg(not(windows))]
    { let _ = (source, destination, expected_source, expected_destination_directory, cancelled); Ok(None) }
}

#[cfg(all(windows, test))]
pub(crate) fn refuse_clipboard_move_rename_once_for_test(source: &Path) {
    windows::refuse_clipboard_move_rename_once_for_test(source);
}

#[cfg(all(windows, test))]
pub(crate) fn clipboard_move_rename_refusal_is_armed_for_test(source: &Path) -> bool {
    windows::clipboard_move_rename_refusal_is_armed_for_test(source)
}

/// Capture the no-follow filesystem object identity and mutation kind for CF_HDROP Move.
pub(crate) fn capture_clipboard_move_source(
    source: &Path,
) -> Option<crate::operations::model::ClipboardMoveSourceEvidence> {
    #[cfg(windows)]
    { windows::capture_clipboard_move_source(source) }
    #[cfg(not(windows))]
    { let _ = source; None }
}

/// Inline rename can reuse the same held-handle proof as a same-volume Move.
/// The exact source and parent identities come from the accepted editor row.
pub(crate) fn rename_sibling_guarded_with_undo(
    source: &Path,
    destination: &Path,
    expected_source: &crate::filesystem_identity::FilesystemIdentity,
    expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
    #[cfg(windows)]
    { windows::rename_sibling_guarded_with_undo(source, destination, expected_source, expected_parent, cancelled, |_| {}) }
    #[cfg(not(windows))]
    { let _ = (source, destination, expected_source, expected_parent, cancelled); Ok(None) }
}

pub(crate) fn move_same_volume_no_replace_worker_observed_with_undo(
    source: &Path,
    destination: &Path,
    expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
    #[cfg(windows)]
    { windows::move_same_volume_no_replace_worker_observed_with_undo(source, destination, expected_destination_directory, cancelled, |_| {}) }
    #[cfg(not(windows))]
    { let _ = (source, destination, expected_destination_directory, cancelled); Ok(None) }
}

pub(crate) fn undo_move_no_replace(receipt: &crate::operations::model::MoveUndoReceipt) -> Result<(), RenameError> {
    #[cfg(windows)]
    { windows::undo_move_no_replace(receipt) }
    #[cfg(not(windows))]
    { let _ = receipt; Err(RenameError::Unsupported) }
}

#[cfg(all(test, windows))]
pub(crate) fn create_test_mount_point_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    windows::create_test_mount_point_junction(link, target)
}

#[cfg(all(test, windows))]
pub(crate) fn remove_test_mount_point_junction(link: &Path) -> std::io::Result<()> {
    windows::remove_test_mount_point_junction(link)
}

/// Copy may cross a volume, but it must never create its staging tree inside
/// the source (including through a junction or another alias of that source).
pub fn validate_directory_copy_target(
    source: &Path,
    destination: &Path,
) -> Result<(), RenameError> {
    #[cfg(target_os = "windows")]
    {
        windows::validate_directory_copy_target(source, destination)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (source, destination);
        Err(RenameError::Unsupported)
    }
}

pub fn rename_in_parent(
    source: &Path,
    new_name: &str,
) -> Result<Option<RenameReceipt>, RenameError> {
    if !is_single_name(new_name) {
        return Err(RenameError::InvalidName);
    }
    let parent = source.parent().ok_or(RenameError::InvalidName)?;
    let destination = parent.join(new_name);
    if source.file_name().is_some_and(|name| name == new_name) {
        return Ok(None);
    }
    // Case-only rename needs a two-phase Windows protocol to avoid an alias
    // accidentally becoming a replace.  Keep it disabled until that protocol
    // has an independent filesystem test.
    if is_case_only_rename(source, new_name) {
        return Err(RenameError::Unsupported);
    }
    #[cfg(target_os = "windows")]
    {
        windows::rename(source, &destination).map(|()| {
            Some(RenameReceipt {
                source: source.to_path_buf(),
                destination,
            })
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = destination;
        Err(RenameError::Unsupported)
    }
}

/// The editor and worker share this Unicode lowercase predicate so their
/// capability gate cannot disagree about whether a rename is case-only.
pub(crate) fn is_case_only_rename(source: &Path, new_name: &str) -> bool {
    source.file_name().is_some_and(|name| {
        let current = name.to_string_lossy();
        current != new_name && current.to_lowercase() == new_name.to_lowercase()
    })
}

pub(crate) use crate::windows_name::is_single_name;

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use ::windows::{
        Win32::{
            Foundation::{BOOLEAN, CloseHandle, HANDLE},
            Storage::FileSystem::{
                CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_FLAGS_AND_ATTRIBUTES, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
                FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ,
                FILE_SHARE_WRITE, FileAttributeTagInfo, FileRenameInfo, GetFileInformationByHandle,
                GetFileInformationByHandleEx, OPEN_EXISTING, SetFileInformationByHandle,
            },
        },
        core::PCWSTR,
    };
    use std::{
        ffi::c_void,
        mem::{offset_of, size_of},
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
    };

    struct Handle(HANDLE);

    #[repr(C)]
    struct NtUnicodeString { length: u16, maximum_length: u16, buffer: *mut u16 }
    #[repr(C)]
    struct NtObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut NtUnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }
    #[repr(C)]
    union NtIoStatusValue { status: i32, pointer: *mut c_void }
    #[repr(C)]
    struct NtIoStatusBlock { status: NtIoStatusValue, information: usize }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(file_handle: *mut HANDLE, desired_access: u32,
            object_attributes: *mut NtObjectAttributes, io_status_block: *mut NtIoStatusBlock,
            allocation_size: *const i64, file_attributes: u32, share_access: u32,
            create_disposition: u32, create_options: u32, ea_buffer: *mut c_void,
            ea_length: u32) -> i32;
        fn NtSetInformationFile(file_handle: HANDLE, io_status_block: *mut NtIoStatusBlock,
            file_information: *mut c_void, length: u32, file_information_class: i32) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    const NT_FILE_OPEN: u32 = 1;
    const NT_FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
    const NT_FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    const NT_FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const NT_FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const NT_OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
    const NT_SYNCHRONIZE: u32 = 0x0010_0000;
    const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;

    struct PinnedRoute { handles: Vec<Handle>, probes: Vec<ObjectProbe>, paths: Vec<PathBuf> }

    pub(super) struct DestinationIdentityLease {
        _handles: Vec<Handle>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct FileIdentity {
        volume: u32,
        index_high: u32,
        index_low: u32,
    }

    impl FileIdentity {
        fn shared(self) -> crate::filesystem_identity::FilesystemIdentity {
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                self.volume,
                (u64::from(self.index_high) << 32) | u64::from(self.index_low),
            )
        }
    }

    struct ObjectProbe {
        identity: FileIdentity,
        is_reparse_point: bool,
        is_directory: bool,
        reparse_tag: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ExactLocation {
        Missing,
        ExpectedSource,
        Occupied,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum CaseOnlyState {
        UnchangedSource,
        StageCollisionWithUnchangedSource,
        StagedSource,
        PublishedDestination,
        Uncertain,
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub(super) fn rename(source: &Path, destination: &Path) -> Result<(), RenameError> {
        let parent = destination.parent().ok_or(RenameError::InvalidName)?;
        // Hold destination parent without FILE_SHARE_DELETE.  This closes the
        // directory-path reuse window while the source handle is renamed.
        let _parent = open(
            parent,
            0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        // DELETE access on this exact opened object makes the rename identity
        // bound to a handle rather than a path re-resolved after validation.
        let source = open(
            source,
            DELETE.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        publish_no_replace(source.0, destination)
    }

    pub(super) fn rename_guarded(
        source_path: &Path,
        destination: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(), RenameError> {
        let parent_path = destination.parent().ok_or(RenameError::InvalidName)?;
        destination
            .file_name()
            .filter(|name| super::is_single_name(&name.to_string_lossy()))
            .ok_or(RenameError::InvalidName)?;
        // RootDirectory=relative failed with ERROR_INVALID_PARAMETER on local
        // NTFS. Instead, hold each no-follow ancestor from volume root downward
        // without FILE_SHARE_DELETE while using the ordinary absolute Win32
        // rename. The order keeps each already-opened prefix stable as the next
        // path component is resolved.
        let (destination_chain, parent_probe, destination_ancestors) =
            open_destination_chain(parent_path, expected_destination_directory)?;

        // 強化したdirectory read pinはsourceのDELETE openと競合するため、祖先一致は保持中のidentityで先に拒否する。
        if destination_ancestors
            .iter()
            .any(|ancestor| ancestor.shared() == *expected_source)
        {
            return Err(RenameError::Unsupported);
        }

        // The same no-follow handle whose identity we validate is the handle
        // passed to SetFileInformationByHandle; a path replacement cannot
        // retarget the mutation.
        let source = open(
            source_path,
            DELETE.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let source_probe = probe_handle(&source)?;
        if source_probe.is_reparse_point {
            return Err(RenameError::ReparsePoint);
        }
        if &source_probe.identity.shared() != expected_source {
            return Err(RenameError::IdentityMismatch);
        }
        if source_probe.identity.volume != parent_probe.identity.volume {
            return Err(RenameError::CrossVolume);
        }
        if source_probe.is_directory
            && destination_ancestors
                .iter()
                .any(|ancestor| *ancestor == source_probe.identity)
        {
            return Err(RenameError::Unsupported);
        }

        // Cancellation may arrive while the ancestor handles are opened.
        // Recheck at the last point before the filesystem mutation.
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RenameError::Cancelled);
        }
        let result = publish_no_replace(source.0, destination);
        drop(destination_chain);
        result
    }

    pub(super) fn probe_reparse_source(
        source_path: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<crate::operations::model::RenameReparseGuard, RenameError> {
        let parent_path = source_path.parent().ok_or(RenameError::InvalidName)?;
        let (_parent_chain, parent_probe, _) =
            open_destination_chain(parent_path, expected_parent)?;
        let source = open(
            source_path,
            0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let probe = probe_handle(&source)?;
        if !probe.is_reparse_point || !matches!(probe.reparse_tag, 0xA000_000C | 0xA000_0003) {
            return Err(RenameError::ReparsePoint);
        }
        if probe.identity.shared() != *expected_source {
            return Err(RenameError::IdentityMismatch);
        }
        Ok(crate::operations::model::RenameReparseGuard {
            source_identity: probe.identity.shared(),
            parent_identity: parent_probe.identity.shared(),
            reparse_tag: probe.reparse_tag,
        })
    }

    pub(super) fn rename_reparse_guarded(
        source_path: &Path,
        destination: &Path,
        guard: &crate::operations::model::RenameReparseGuard,
        expected_kind: Option<crate::directory::EntryKind>,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(), RenameError> {
        let parent_path = destination.parent().ok_or(RenameError::InvalidName)?;
        destination
            .file_name()
            .filter(|name| super::is_single_name(&name.to_string_lossy()))
            .ok_or(RenameError::InvalidName)?;
        let (_parent_chain, parent_probe, destination_ancestors) =
            open_destination_chain(parent_path, &guard.parent_identity)?;
        // reparse data は不透明なので、tag/identity確認から rename 完了まで同じ object を保つ。
        let source = open_with_share(
            source_path,
            DELETE.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ,
        )?;
        let probe = probe_handle(&source)?;
        if !probe.is_reparse_point
            || !matches!(guard.reparse_tag, 0xA000_000C | 0xA000_0003)
            || probe.reparse_tag != guard.reparse_tag
            || expected_kind.is_some_and(|kind| {
                probe.is_directory != (kind == crate::directory::EntryKind::Folder)
            })
            || (guard.reparse_tag == 0xA000_0003
                && expected_kind.is_some_and(|kind| kind != crate::directory::EntryKind::Folder))
        {
            return Err(RenameError::ReparsePoint);
        }
        if probe.identity.shared() != guard.source_identity
            || parent_probe.identity.shared() != guard.parent_identity
            || probe.identity.volume != parent_probe.identity.volume
        {
            return Err(RenameError::IdentityMismatch);
        }
        if destination_ancestors
            .iter()
            .any(|ancestor| *ancestor == probe.identity)
        {
            return Err(RenameError::Unsupported);
        }
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RenameError::Cancelled);
        }
        publish_no_replace(source.0, destination)
    }

    pub(super) fn rename_case_only_guarded(
        source_path: &Path,
        destination: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_parent: &crate::filesystem_identity::FilesystemIdentity,
        reparse_tag: Option<u32>,
        expected_kind: Option<crate::directory::EntryKind>,
        token: crate::operations::model::AttemptToken,
        item: crate::operations::model::ItemId,
        cancelled: &std::sync::atomic::AtomicBool,
        controls: &std::sync::mpsc::Receiver<crate::operations::executor::WorkerControl>,
        emit: &mut impl FnMut(crate::operations::executor::ExecutorFact),
    ) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
        let parent = source_path.parent().ok_or(RenameError::InvalidName)?;
        let destination_parent = destination.parent().ok_or(RenameError::InvalidName)?;
        if parent != destination_parent
            || !super::is_case_only_rename(
                source_path,
                destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(RenameError::InvalidName)?,
            )
        {
            return Err(RenameError::InvalidName);
        }
        let destination_name = destination
            .file_name()
            .filter(|name| super::is_single_name(&name.to_string_lossy()))
            .ok_or(RenameError::InvalidName)?;
        let source_token = identity_token(expected_source)?;
        let parent_token = identity_token(expected_parent)?;
        let (parent_chain, parent_probe, _) = open_destination_chain(parent, expected_parent)?;
        let parent_route = route_proof(&open_relative_route(parent)?);
        let source = open_with_share(
            source_path,
            DELETE.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            if reparse_tag.is_some() {
                FILE_SHARE_READ
            } else {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            },
        )?;
        let source_probe = probe_handle(&source)?;
        let supported_reparse = source_probe.is_reparse_point
            && matches!(source_probe.reparse_tag, 0xA000_000C | 0xA000_0003)
            && reparse_tag == Some(source_probe.reparse_tag)
            && expected_kind.is_none_or(|kind| {
                source_probe.is_directory == (kind == crate::directory::EntryKind::Folder)
                    && (source_probe.reparse_tag != 0xA000_0003
                        || kind == crate::directory::EntryKind::Folder)
            });
        if (source_probe.is_reparse_point && !supported_reparse)
            || (!source_probe.is_reparse_point && reparse_tag.is_some())
        {
            return Err(RenameError::ReparsePoint);
        }
        if expected_kind.is_some_and(|kind| {
            source_probe.is_directory != (kind == crate::directory::EntryKind::Folder)
        }) {
            return Err(RenameError::IdentityMismatch);
        }
        if source_probe.identity.shared() != *expected_source {
            return Err(RenameError::IdentityMismatch);
        }
        if source_probe.identity.volume != parent_probe.identity.volume {
            return Err(RenameError::CrossVolume);
        }
        let undo_case_only_object = !source_probe.is_reparse_point;
        let undo_route = parent_route;

        for attempt in 0..128_u32 {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(RenameError::Cancelled);
            }
            let stage = parent.join(format!(
                ".gpui-filer-rename-{}-{}-{attempt:03x}.tmp",
                token.job_id().value(),
                token.attempt_id().value(),
            ));
            wait_for_rename_stage_intent(
                token,
                item,
                source_path,
                &stage,
                destination,
                &source_token,
                &parent_token,
                1,
                controls,
                emit,
            )?;
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(RenameError::Cancelled);
            }
            match publish_no_replace(source.0, &stage) {
                Ok(()) => {
                    return complete_case_only_stage(
                        &source,
                        source_path,
                        &stage,
                        destination,
                        destination_name,
                        expected_source,
                        token,
                        item,
                        &source_token,
                        &parent_token,
                        cancelled,
                        controls,
                        emit,
                    ).map(|()| undo_case_only_object.then(|| crate::operations::model::MoveUndoReceipt {
                        source: source_path.to_path_buf(),
                        destination: destination.to_path_buf(),
                        object_identity: expected_source.clone(),
                        object_kind: crate::operations::model::MoveUndoObjectKind::CaseOnlyRename,
                        source_parent_route: undo_route.clone(),
                        destination_parent_route: undo_route.clone(),
                        cross_volume_file: None,
                    }));
                }
                Err(error) => {
                    match case_only_state(source_path, &stage, destination, expected_source) {
                        Ok(CaseOnlyState::StagedSource) => {
                            return complete_case_only_stage(
                                &source,
                                source_path,
                                &stage,
                                destination,
                                destination_name,
                                expected_source,
                                token,
                                item,
                                &source_token,
                                &parent_token,
                                cancelled,
                                controls,
                                emit,
                            ).map(|()| undo_case_only_object.then(|| crate::operations::model::MoveUndoReceipt {
                                source: source_path.to_path_buf(),
                                destination: destination.to_path_buf(),
                                object_identity: expected_source.clone(),
                                object_kind: crate::operations::model::MoveUndoObjectKind::CaseOnlyRename,
                                source_parent_route: undo_route.clone(),
                                destination_parent_route: undo_route.clone(),
                                cross_volume_file: None,
                            }));
                        }
                        Ok(CaseOnlyState::StageCollisionWithUnchangedSource)
                            if matches!(error, RenameError::DestinationExists) =>
                        {
                            // The generated sibling is occupied. The next loop
                            // iteration journals and acknowledges a fresh exact path.
                            continue;
                        }
                        Ok(CaseOnlyState::UnchangedSource) => {
                            return Err(error);
                        }
                        _ => {
                            return Err(RenameError::Partial(format!(
                                "case-only rename stage location is uncertain; source={}, stage={}, destination={}, source_identity={source_token}, parent_identity={parent_token}",
                                source_path.display(),
                                stage.display(),
                                destination.display(),
                            )));
                        }
                    }
                }
            }
        }
        drop(parent_chain);
        Err(RenameError::DestinationExists)
    }

    fn complete_case_only_stage(
        source: &Handle,
        source_path: &Path,
        stage: &Path,
        destination: &Path,
        _destination_name: &std::ffi::OsStr,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        token: crate::operations::model::AttemptToken,
        item: crate::operations::model::ItemId,
        source_token: &str,
        parent_token: &str,
        cancelled: &std::sync::atomic::AtomicBool,
        controls: &std::sync::mpsc::Receiver<crate::operations::executor::WorkerControl>,
        emit: &mut impl FnMut(crate::operations::executor::ExecutorFact),
    ) -> Result<(), RenameError> {
        let rollback = |detail: String| -> Result<(), RenameError> {
            let was_cancelled = cancelled.load(std::sync::atomic::Ordering::Acquire);
            match (
                case_only_location(stage, expected_source),
                case_only_location(source_path, expected_source),
            ) {
                (Ok(ExactLocation::ExpectedSource), Ok(ExactLocation::Missing)) => {
                    // Keep the held source handle; path checks only establish where
                    // that exact object currently lives before rollback. An
                    // unrelated exact destination spelling does not occupy the
                    // source spelling in a case-sensitive directory.
                    let _ = publish_no_replace(source.0, source_path);
                }
                (Ok(ExactLocation::Missing), Ok(ExactLocation::ExpectedSource)) => {
                    return if was_cancelled {
                        Err(RenameError::Cancelled)
                    } else {
                        Err(RenameError::Io(detail))
                    };
                }
                _ => {
                    return Err(partial_case_only(
                        source_path,
                        stage,
                        destination,
                        source_token,
                        parent_token,
                        &detail,
                    ));
                }
            }
            match (
                case_only_location(source_path, expected_source),
                case_only_location(stage, expected_source),
            ) {
                (Ok(ExactLocation::ExpectedSource), Ok(ExactLocation::Missing)) => {
                    if was_cancelled {
                        Err(RenameError::Cancelled)
                    } else {
                        Err(RenameError::Io(detail))
                    }
                }
                _ => Err(partial_case_only(
                    source_path,
                    stage,
                    destination,
                    source_token,
                    parent_token,
                    &detail,
                )),
            }
        };

        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return rollback("cancelled after staging".into());
        }
        if let Err(error) = wait_for_rename_stage_intent(
            token,
            item,
            source_path,
            stage,
            destination,
            source_token,
            parent_token,
            2,
            controls,
            emit,
        ) {
            return rollback(format!("could not journal final stage: {error:?}"));
        }
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return rollback("cancelled before publish".into());
        }
        match publish_no_replace(source.0, destination) {
            Ok(()) => Ok(()),
            Err(error) => match case_only_state(source_path, stage, destination, expected_source) {
                Ok(CaseOnlyState::PublishedDestination) => Ok(()),
                Ok(CaseOnlyState::StagedSource) => {
                    rollback(format!("final publish failed: {error:?}"))
                }
                Ok(CaseOnlyState::UnchangedSource) => rollback(format!(
                    "final publish left the source at its original spelling: {error:?}"
                )),
                _ => rollback(format!("final publish location uncertain: {error:?}")),
            },
        }
    }

    fn wait_for_rename_stage_intent(
        token: crate::operations::model::AttemptToken,
        item: crate::operations::model::ItemId,
        source: &Path,
        stage: &Path,
        destination: &Path,
        source_identity: &str,
        parent_identity: &str,
        phase: u8,
        controls: &std::sync::mpsc::Receiver<crate::operations::executor::WorkerControl>,
        emit: &mut impl FnMut(crate::operations::executor::ExecutorFact),
    ) -> Result<(), RenameError> {
        emit(
            crate::operations::executor::ExecutorFact::RenameStageIntent {
                token,
                item,
                source: source.to_path_buf(),
                stage: stage.to_path_buf(),
                destination: destination.to_path_buf(),
                source_identity: source_identity.into(),
                parent_identity: parent_identity.into(),
                phase,
            },
        );
        match controls.recv() {
            Ok(crate::operations::executor::WorkerControl::RenameStageJournaledExact {
                token: actual_token,
                item: actual_item,
                source: actual_source,
                stage: actual_stage,
                destination: actual_destination,
                source_identity: actual_source_identity,
                parent_identity: actual_parent_identity,
                phase: actual_phase,
            }) if actual_token == token
                && actual_item == item
                && actual_source == source
                && actual_stage == stage
                && actual_destination == destination
                && actual_source_identity == source_identity
                && actual_parent_identity == parent_identity
                && actual_phase == phase =>
            {
                Ok(())
            }
            _ => Err(RenameError::Cancelled),
        }
    }

    fn case_only_location(
        path: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<ExactLocation, RenameError> {
        let parent = path.parent().ok_or(RenameError::InvalidName)?;
        let name = path.file_name().ok_or(RenameError::InvalidName)?;
        let entries = std::fs::read_dir(parent).map_err(io)?;
        for entry in entries {
            let entry = entry.map_err(io)?;
            if entry.file_name() == name {
                let handle = open_with_share(
                    &entry.path(),
                    0,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                )?;
                return probe_handle(&handle).map(|probe| {
                    if probe.identity.shared() == *expected_source {
                        ExactLocation::ExpectedSource
                    } else {
                        ExactLocation::Occupied
                    }
                });
            }
        }
        Ok(ExactLocation::Missing)
    }

    fn case_only_state(
        source: &Path,
        stage: &Path,
        destination: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<CaseOnlyState, RenameError> {
        let source = case_only_location(source, expected_source)?;
        let stage = case_only_location(stage, expected_source)?;
        let destination = case_only_location(destination, expected_source)?;
        Ok(match (source, stage, destination) {
            (ExactLocation::ExpectedSource, ExactLocation::Missing, ExactLocation::Missing) => {
                CaseOnlyState::UnchangedSource
            }
            (ExactLocation::ExpectedSource, ExactLocation::Occupied, ExactLocation::Missing) => {
                CaseOnlyState::StageCollisionWithUnchangedSource
            }
            (ExactLocation::Missing, ExactLocation::ExpectedSource, ExactLocation::Missing) => {
                CaseOnlyState::StagedSource
            }
            (ExactLocation::Missing, ExactLocation::Missing, ExactLocation::ExpectedSource) => {
                CaseOnlyState::PublishedDestination
            }
            _ => CaseOnlyState::Uncertain,
        })
    }

    fn partial_case_only(
        source: &Path,
        stage: &Path,
        destination: &Path,
        source_token: &str,
        parent_token: &str,
        detail: &str,
    ) -> RenameError {
        RenameError::Partial(format!(
            "case-only rename has an unresolved exact-spelling state; source={}, stage={}, destination={}, source_identity={source_token}, parent_identity={parent_token}; {detail}",
            source.display(),
            stage.display(),
            destination.display(),
        ))
    }

    fn identity_token(
        identity: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<String, RenameError> {
        let (volume, index) = identity.windows_parts().ok_or(RenameError::Unsupported)?;
        Ok(format!("{volume:08x}:{index:016x}"))
    }

    pub(super) fn pin_destination_directory_identity(
        path: &Path,
        expected_identity: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<DestinationIdentityLease, RenameError> {
        // 絶対pathでのCopy中にtargetやancestorが差し替わらないよう、全handleをleaseへ残す。
        let (handles, _, _) = open_destination_chain(path, expected_identity)?;
        Ok(DestinationIdentityLease { _handles: handles })
    }

    pub(super) fn rename_held_file_no_replace(
        source: &std::fs::File,
        destination: &Path,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<(), RenameError> {
        let parent = destination.parent().ok_or(RenameError::InvalidName)?;
        destination
            .file_name()
            .filter(|name| super::is_single_name(&name.to_string_lossy()))
            .ok_or(RenameError::InvalidName)?;
        // Keep the full route pinned while using the NTFS-tested absolute rename form.
        let (_chain, _probe, _ancestors) =
            open_destination_chain(parent, expected_destination_directory)?;
        let source = HANDLE(source.as_raw_handle() as *mut _);
        publish_no_replace(source, destination)
    }

    fn open_destination_chain(
        parent_path: &Path,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
    ) -> Result<(Vec<Handle>, ObjectProbe, Vec<FileIdentity>), RenameError> {
        if !parent_path.is_absolute() {
            return Err(RenameError::Unsupported);
        }
        let mut paths = parent_path.ancestors().collect::<Vec<_>>();
        paths.reverse();
        let mut handles = Vec::with_capacity(paths.len());
        let mut identities = Vec::with_capacity(paths.len());
        let mut leaf_probe: Option<ObjectProbe> = None;
        for path in paths {
            // Access権限がないhandleは削除共有の競合を防げないため、既存のdirectory pinと同じ読取権限を求める。
            let handle = open(
                path,
                FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )?;
            let probe = probe_handle(&handle)?;
            if probe.is_reparse_point {
                return Err(RenameError::ReparsePoint);
            }
            if !probe.is_directory {
                return Err(RenameError::Unsupported);
            }
            identities.push(probe.identity);
            leaf_probe = Some(probe);
            handles.push(handle);
        }
        let probe = leaf_probe.ok_or(RenameError::Unsupported)?;
        if &probe.identity.shared() != expected_destination_directory {
            return Err(RenameError::IdentityMismatch);
        }
        Ok((handles, probe, identities))
    }

    pub(super) fn move_same_volume_no_replace_guarded_with_undo(
        source_path: &Path,
        destination: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &std::sync::atomic::AtomicBool,
        before_native: impl FnOnce(HANDLE),
    ) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
        move_same_volume_no_replace_with_expected_source(source_path, destination, expected_destination_directory, cancelled, before_native, Some(expected_source), false, false)
    }

    pub(super) fn move_same_volume_no_replace_clipboard_with_undo(
        source_path: &Path,
        destination: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
        move_same_volume_no_replace_with_expected_source(source_path, destination, expected_destination_directory, cancelled, |_| {}, Some(expected_source), false, true)
    }

    pub(super) fn capture_clipboard_move_source(
        source_path: &Path,
    ) -> Option<crate::operations::model::ClipboardMoveSourceEvidence> {
        let source = open(
            source_path,
            0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        ).ok()?;
        let probe = probe_handle(&source).ok()?;
        let reparse_tag = if probe.is_reparse_point {
            Some(matches!(probe.reparse_tag, 0xA000_000C | 0xA000_0003).then_some(probe.reparse_tag)?)
        } else {
            None
        };
        let source_kind = if probe.is_directory {
            crate::directory::EntryKind::Folder
        } else {
            crate::directory::EntryKind::File
        };
        Some(crate::operations::model::ClipboardMoveSourceEvidence {
            source: source_path.to_path_buf(),
            source_identity: probe.identity.shared(),
            source_kind,
            reparse_tag,
        })
    }

    pub(super) fn rename_sibling_guarded_with_undo(
        source_path: &Path,
        destination: &Path,
        expected_source: &crate::filesystem_identity::FilesystemIdentity,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &std::sync::atomic::AtomicBool,
        before_native: impl FnOnce(HANDLE),
    ) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
        move_same_volume_no_replace_with_expected_source(source_path, destination, expected_destination_directory, cancelled, before_native, Some(expected_source), true, false)
    }

    pub(super) fn move_same_volume_no_replace_worker_observed_with_undo(
        source_path: &Path,
        destination: &Path,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &std::sync::atomic::AtomicBool,
        before_native: impl FnOnce(HANDLE),
    ) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
        move_same_volume_no_replace_with_expected_source(source_path, destination, expected_destination_directory, cancelled, before_native, None, false, false)
    }

    fn move_same_volume_no_replace_with_expected_source(
        source_path: &Path,
        destination: &Path,
        expected_destination_directory: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: &std::sync::atomic::AtomicBool,
        before_native: impl FnOnce(HANDLE),
        expected_source: Option<&crate::filesystem_identity::FilesystemIdentity>,
        require_sibling_parent: bool,
        preserve_cross_volume: bool,
    ) -> Result<Option<crate::operations::model::MoveUndoReceipt>, RenameError> {
        let prepared = (|| {
            let source_name = source_path.file_name().and_then(|name| name.to_str()).ok_or(RenameError::InvalidName)?;
            let destination_name = destination.file_name().and_then(|name| name.to_str()).ok_or(RenameError::InvalidName)?;
            if !super::is_single_name(source_name) || !super::is_single_name(destination_name) || source_path == destination {
                return Err(RenameError::Unsupported);
            }
            let source_parent = source_path.parent().ok_or(RenameError::InvalidName)?;
            let destination_parent = destination.parent().ok_or(RenameError::InvalidName)?;
            if require_sibling_parent && source_parent != destination_parent {
                return Err(RenameError::IdentityMismatch);
            }
            let source_route = open_relative_route(source_parent)?;
            let destination_route = open_relative_route(destination_parent)?;
            let source_parent_probe = source_route.probes.last().ok_or(RenameError::Unsupported)?;
            let destination_parent_probe = destination_route.probes.last().ok_or(RenameError::Unsupported)?;
            if destination_parent_probe.identity.shared() != *expected_destination_directory
                || (require_sibling_parent && source_parent_probe.identity.shared() != *expected_destination_directory) {
                return Err(RenameError::IdentityMismatch);
            }
            if cancelled.load(std::sync::atomic::Ordering::Acquire) { return Err(RenameError::Cancelled); }
            let source_parent_handle = source_route.handles.last().ok_or(RenameError::Unsupported)?;
            let source = nt_open_relative(source_parent_handle, source_path.file_name().ok_or(RenameError::InvalidName)?, None)?;
            let source_probe = probe_handle(&source)?;
            let object_kind = if source_probe.is_reparse_point {
                if !matches!(source_probe.reparse_tag, 0xA000_000C | 0xA000_0003)
                    || (source_probe.reparse_tag == 0xA000_0003 && !source_probe.is_directory)
                {
                    return Err(RenameError::Unsupported);
                }
                crate::operations::model::MoveUndoObjectKind::ReparsePoint {
                    tag: source_probe.reparse_tag,
                    is_directory: source_probe.is_directory,
                }
            } else if source_probe.is_directory {
                crate::operations::model::MoveUndoObjectKind::Directory
            } else {
                crate::operations::model::MoveUndoObjectKind::RegularFile
            };
            if expected_source.is_some_and(|expected| source_probe.identity.shared() != *expected) { return Err(RenameError::IdentityMismatch); }
            if source_probe.identity.volume != source_parent_probe.identity.volume
                || source_probe.identity.volume != destination_parent_probe.identity.volume
            { return Err(RenameError::CrossVolume); }
            if cancelled.load(std::sync::atomic::Ordering::Acquire) { return Err(RenameError::Cancelled); }
            let receipt = crate::operations::model::MoveUndoReceipt {
                source: source_path.to_path_buf(), destination: destination.to_path_buf(),
                object_identity: source_probe.identity.shared(),
                object_kind,
                source_parent_route: route_proof(&source_route),
                destination_parent_route: route_proof(&destination_route),
                cross_volume_file: None,
            };
            Ok((source, source_route, destination_route, destination_name.to_owned(), receipt))
        })();
        let (source, source_route, destination_route, destination_name, receipt) = match prepared {
            Ok(prepared) => prepared,
            Err(RenameError::Cancelled) => return Err(RenameError::Cancelled),
            Err(RenameError::IdentityMismatch) => return Err(RenameError::IdentityMismatch),
            Err(RenameError::CrossVolume) if preserve_cross_volume => return Err(RenameError::CrossVolume),
            // 取り消し不能なnative mutation前の証明不足は既存のguarded Moveへ委ねる。
            Err(_) => return Ok(None),
        };
        let _source_route = source_route;
        let destination_parent_handle = destination_route.handles.last().ok_or(RenameError::Unsupported)?.0;
        before_native(destination_parent_handle);
        #[cfg(test)]
        if refuse_clipboard_move_rename_once_for_exact_source(source_path) {
            return Err(RenameError::Io("injected exact-source refusal before native rename".into()));
        }
        native_rename_relative_no_replace(source.0, destination_parent_handle, &destination_name)?;
        Ok(Some(receipt))
    }

    #[cfg(test)]
    fn clipboard_move_rename_refusal_paths() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
        static PATHS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
            std::sync::OnceLock::new();
        PATHS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
    }

    #[cfg(test)]
    pub(crate) fn refuse_clipboard_move_rename_once_for_test(source: &Path) {
        let inserted = clipboard_move_rename_refusal_paths().lock().unwrap().insert(source.to_path_buf());
        assert!(inserted, "an exact-source rename refusal is already armed");
    }

    #[cfg(test)]
    pub(crate) fn clipboard_move_rename_refusal_is_armed_for_test(source: &Path) -> bool {
        clipboard_move_rename_refusal_paths().lock().unwrap().contains(source)
    }

    #[cfg(test)]
    fn refuse_clipboard_move_rename_once_for_exact_source(source: &Path) -> bool {
        clipboard_move_rename_refusal_paths().lock().unwrap().remove(source)
    }

    pub(crate) fn undo_move_no_replace(receipt: &crate::operations::model::MoveUndoReceipt) -> Result<(), RenameError> {
        if receipt.object_kind == crate::operations::model::MoveUndoObjectKind::CaseOnlyRename {
            return undo_case_only_rename(receipt);
        }
        let source_parent = receipt.source.parent().ok_or(RenameError::InvalidName)?;
        let destination_parent = receipt.destination.parent().ok_or(RenameError::InvalidName)?;
        let source_route = open_relative_route(source_parent)?;
        let destination_route = open_relative_route(destination_parent)?;
        if route_proof(&source_route) != receipt.source_parent_route
            || route_proof(&destination_route) != receipt.destination_parent_route
        { return Err(RenameError::IdentityMismatch); }
        let (is_directory, expected_reparse_tag) = match receipt.object_kind {
            crate::operations::model::MoveUndoObjectKind::RegularFile => (false, None),
            crate::operations::model::MoveUndoObjectKind::Directory => (true, None),
            // Case-only Rename Undo has a dedicated no-replace path and must
            // never fall through to Move's Keep Both behavior.
            crate::operations::model::MoveUndoObjectKind::CaseOnlyRename => {
                return Err(RenameError::Unsupported);
            }
            crate::operations::model::MoveUndoObjectKind::ReparsePoint { tag, is_directory }
                if matches!(tag, 0xA000_000C | 0xA000_0003)
                    && (tag != 0xA000_0003 || is_directory) => (is_directory, Some(tag)),
            crate::operations::model::MoveUndoObjectKind::ReparsePoint { .. } => {
                return Err(RenameError::IdentityMismatch);
            }
        };
        let destination_parent_handle = destination_route.handles.last().ok_or(RenameError::Unsupported)?;
        let destination_leaf = receipt.destination.file_name().ok_or(RenameError::InvalidName)?;
        if !nt_relative_child_exists_no_follow(destination_parent_handle, destination_leaf)? {
            // v5 は Move 元が既に消えていれば Undo を読み飛ばしていた。
            // 親 route を検証した後の exact leaf absence だけを同じ no-op として消費する。
            return Ok(());
        }
        let moved = nt_open_relative_for_delete(destination_parent_handle, destination_leaf, Some(is_directory))?;
        let probe = probe_handle(&moved)?;
        if probe.is_reparse_point != expected_reparse_tag.is_some()
            || probe.is_reparse_point && probe.reparse_tag != expected_reparse_tag.unwrap_or_default()
            || probe.is_directory != is_directory
            || probe.identity.shared() != receipt.object_identity
        {
            return Err(RenameError::IdentityMismatch);
        }
        let source_parent_handle = source_route.handles.last().ok_or(RenameError::Unsupported)?.0;
        let original_name = receipt.source.file_name().and_then(|name| name.to_str()).ok_or(RenameError::InvalidName)?;
        match native_rename_relative_no_replace(moved.0, source_parent_handle, original_name) {
            Ok(()) => Ok(()),
            Err(RenameError::DestinationExists) => {
                // 衝突と明示された場合だけ、同じ保持ハンドルから次の名前を原子的に試す。
                for index in 2..=999 {
                    let candidate = move_undo_keep_both_name(
                        original_name,
                        index,
                        matches!(receipt.object_kind,
                            crate::operations::model::MoveUndoObjectKind::Directory
                                | crate::operations::model::MoveUndoObjectKind::ReparsePoint { is_directory: true, .. }),
                    )?;
                    match native_rename_relative_no_replace(moved.0, source_parent_handle, &candidate) {
                        Ok(()) => return Ok(()),
                        Err(RenameError::DestinationExists) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(RenameError::DestinationExists)
            }
            Err(error) => Err(error),
        }
    }

    fn undo_case_only_rename(
        receipt: &crate::operations::model::MoveUndoReceipt,
    ) -> Result<(), RenameError> {
        if receipt.cross_volume_file.is_some()
            || receipt.source.parent() != receipt.destination.parent()
            || receipt.source.file_name().and_then(|name| name.to_str()).is_none_or(|name| {
                !super::is_case_only_rename(&receipt.destination, name)
            })
        {
            return Err(RenameError::IdentityMismatch);
        }
        let expected_identity = &receipt.object_identity;
        let parent = receipt.source.parent().ok_or(RenameError::InvalidName)?;
        let route = open_relative_route(parent)?;
        if route_proof(&route) != receipt.source_parent_route
            || route_proof(&route) != receipt.destination_parent_route
        {
            return Err(RenameError::IdentityMismatch);
        }
        let parent_handle = route.handles.last().ok_or(RenameError::Unsupported)?;
        let source_name = receipt.destination.file_name().ok_or(RenameError::InvalidName)?;
        let original_name = receipt.source.file_name().and_then(|name| name.to_str()).ok_or(RenameError::InvalidName)?;
        let current_name = source_name.to_str().ok_or(RenameError::InvalidName)?;
        let identity = identity_token(expected_identity)?;
        let parent_identity = identity_token(&route.probes.last().ok_or(RenameError::Unsupported)?.identity.shared())?;
        let current_location = case_only_location(&receipt.destination, expected_identity)?;
        let original_location = case_only_location(&receipt.source, expected_identity)?;
        if current_location == ExactLocation::Missing && original_location == ExactLocation::ExpectedSource {
            return Ok(());
        }
        if original_location != ExactLocation::Missing {
            return Err(RenameError::DestinationExists);
        }
        let mut stage = None;
        for attempt in 0..128_u32 {
            let name = format!(".gpui-filer-undo-{identity}-{attempt:03x}.tmp");
            let path = parent.join(&name);
            match (current_location, case_only_location(&path, expected_identity)?) {
                (ExactLocation::ExpectedSource, ExactLocation::Missing)
                | (ExactLocation::Missing, ExactLocation::ExpectedSource) => {
                    stage = Some(name);
                    break;
                }
                _ => continue,
            }
        }
        let (stage_name, moved, already_staged) = match (current_location, stage) {
            (ExactLocation::ExpectedSource, Some(stage_name)) => {
                let moved = nt_open_relative_for_delete(parent_handle, source_name, None)?;
                if probe_handle(&moved)?.identity.shared() != *expected_identity {
                    return Err(RenameError::IdentityMismatch);
                }
                (stage_name, moved, false)
            }
            (ExactLocation::Missing, Some(stage_name)) => {
                let stage_leaf = std::ffi::OsStr::new(&stage_name);
                let moved = nt_open_relative_for_delete(parent_handle, stage_leaf, None)?;
                if probe_handle(&moved)?.identity.shared() != *expected_identity {
                    return Err(RenameError::IdentityMismatch);
                }
                (stage_name, moved, true)
            }
            _ => return Err(RenameError::IdentityMismatch),
        };
        let stage_path = parent.join(&stage_name);
        if !already_staged {
            match native_rename_relative_no_replace(moved.0, parent_handle.0, &stage_name) {
                Ok(()) => {}
                Err(error) => return Err(error),
            }
        }
        match native_rename_relative_no_replace(moved.0, parent_handle.0, original_name) {
            Ok(()) => Ok(()),
            Err(final_error) => {
                let state = case_only_state(&receipt.destination, &stage_path, &receipt.source, expected_identity)
                    .map_err(|error| partial_case_only(
                        &receipt.destination, &stage_path, &receipt.source, &identity, &parent_identity,
                        &format!("Undo publish failed ({final_error:?}); exact state could not be read ({error:?})"),
                    ))?;
                match state {
                    CaseOnlyState::PublishedDestination => Ok(()),
                    CaseOnlyState::StagedSource => {
                        match native_rename_relative_no_replace(moved.0, parent_handle.0, current_name) {
                            Ok(()) => Err(final_error),
                            Err(rollback_error) => Err(partial_case_only(
                                &receipt.destination, &stage_path, &receipt.source, &identity, &parent_identity,
                                &format!("Undo publish failed ({final_error:?}) and exact rollback failed ({rollback_error:?})"),
                            )),
                        }
                    }
                    state => Err(partial_case_only(
                        &receipt.destination, &stage_path, &receipt.source, &identity, &parent_identity,
                        &format!("Undo publish failed ({final_error:?}); exact state={state:?}"),
                    )),
                }
            }
        }
    }

    fn move_undo_keep_both_name(original: &str, index: u16, is_directory: bool) -> Result<String, RenameError> {
        if is_directory {
            return Ok(format!("{original} ({index})"));
        }
        let path = Path::new(original);
        let stem = path.file_stem().and_then(|value| value.to_str()).ok_or(RenameError::InvalidName)?;
        let extension = path.extension().and_then(|value| value.to_str());
        Ok(match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{original} ({index})"),
        })
    }

    fn open_relative_route(parent: &Path) -> Result<PinnedRoute, RenameError> {
        use std::path::{Component, Prefix};
        if !parent.is_absolute() { return Err(RenameError::Unsupported); }
        let mut components = parent.components();
        let prefix = match components.next() {
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => prefix,
            _ => return Err(RenameError::Unsupported),
        };
        if !matches!(components.next(), Some(Component::RootDir)) { return Err(RenameError::Unsupported); }
        let mut root_text = prefix.as_os_str().to_os_string();
        root_text.push("\\");
        let root_path = PathBuf::from(root_text);
        // Route handles only pin identities. The kernel opens the destination parent
        // internally for the relative rename, so these handles need no write grant.
        let root = open_with_share(&root_path,
            FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)?;
        let root_probe = probe_handle(&root)?;
        if root_probe.is_reparse_point || !root_probe.is_directory { return Err(RenameError::ReparsePoint); }
        let mut route = PinnedRoute { handles: vec![root], probes: vec![root_probe], paths: vec![root_path] };
        for component in components {
            let Component::Normal(name) = component else { return Err(RenameError::Unsupported); };
            let handle = nt_open_relative(route.handles.last().ok_or(RenameError::Unsupported)?, name, Some(true))?;
            let probe = probe_handle(&handle)?;
            if probe.is_reparse_point || !probe.is_directory { return Err(RenameError::ReparsePoint); }
            route.paths.push(route.paths.last().unwrap().join(name));
            route.probes.push(probe);
            route.handles.push(handle);
        }
        Ok(route)
    }

    fn route_proof(route: &PinnedRoute) -> Vec<crate::operations::model::SourceCleanupParentIdentity> {
        route.paths.iter().zip(&route.probes).map(|(path, probe)|
            crate::operations::model::SourceCleanupParentIdentity { path: path.clone(), identity: probe.identity.shared() }
        ).collect()
    }

    fn nt_open_relative(parent: &Handle, name: &std::ffi::OsStr, directory: Option<bool>) -> Result<Handle, RenameError> {
        let mut wide = name.encode_wide().collect::<Vec<_>>();
        let bytes = u16::try_from(wide.len().checked_mul(size_of::<u16>()).ok_or(RenameError::InvalidName)?)
            .map_err(|_| RenameError::InvalidName)?;
        let mut unicode = NtUnicodeString { length: bytes, maximum_length: bytes, buffer: wide.as_mut_ptr() };
        let mut attributes = NtObjectAttributes { length: size_of::<NtObjectAttributes>() as u32,
            root_directory: parent.0, object_name: &mut unicode, attributes: NT_OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null_mut(), security_quality_of_service: std::ptr::null_mut() };
        let mut iosb = NtIoStatusBlock { status: NtIoStatusValue { status: 0 }, information: 0 };
        let mut handle = HANDLE::default();
        let access = FILE_READ_ATTRIBUTES.0 | if directory == Some(true) {
            FILE_LIST_DIRECTORY.0
        } else { DELETE.0 } | NT_SYNCHRONIZE;
        let options = NT_FILE_OPEN_REPARSE_POINT | NT_FILE_SYNCHRONOUS_IO_NONALERT
            | match directory { Some(true) => NT_FILE_DIRECTORY_FILE, Some(false) => NT_FILE_NON_DIRECTORY_FILE, None => 0 };
        // Parent HANDLEをRootDirectoryに使い、pathの再解決を避ける。
        let status = unsafe { NtCreateFile(&mut handle, access, &mut attributes, &mut iosb, std::ptr::null(),
            0, (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0, NT_FILE_OPEN, options,
            std::ptr::null_mut(), 0) };
        let io_status = unsafe { iosb.status.status };
        if status != 0 || io_status != 0 {
            let code = if status < 0 { unsafe { RtlNtStatusToDosError(status) } } else { status as u32 };
            return Err(RenameError::Io(format!("NtCreateFile relative open failed: NTSTATUS 0x{:08x}, error {code}", status as u32)));
        }
        Ok(Handle(handle))
    }

    fn nt_open_relative_for_delete(parent: &Handle, name: &std::ffi::OsStr, directory: Option<bool>) -> Result<Handle, RenameError> {
        nt_open_relative_child_if_present(
            parent,
            name,
            directory,
            FILE_READ_ATTRIBUTES.0 | DELETE.0 | NT_SYNCHRONIZE,
            "relative delete open",
        )?.ok_or_else(|| {
            RenameError::Io("NtCreateFile relative delete open failed: destination leaf is absent".into())
        })
    }

    fn nt_relative_child_exists_no_follow(parent: &Handle, name: &std::ffi::OsStr) -> Result<bool, RenameError> {
        Ok(nt_open_relative_child_if_present(
            parent,
            name,
            None,
            FILE_READ_ATTRIBUTES.0 | NT_SYNCHRONIZE,
            "relative no-follow child probe",
        )?.is_some())
    }

    fn nt_open_relative_child_if_present(
        parent: &Handle,
        name: &std::ffi::OsStr,
        directory: Option<bool>,
        access: u32,
        operation: &str,
    ) -> Result<Option<Handle>, RenameError> {
        let mut wide = name.encode_wide().collect::<Vec<_>>();
        let bytes = u16::try_from(wide.len().checked_mul(size_of::<u16>()).ok_or(RenameError::InvalidName)?)
            .map_err(|_| RenameError::InvalidName)?;
        let mut unicode = NtUnicodeString { length: bytes, maximum_length: bytes, buffer: wide.as_mut_ptr() };
        let mut attributes = NtObjectAttributes { length: size_of::<NtObjectAttributes>() as u32,
            root_directory: parent.0, object_name: &mut unicode, attributes: NT_OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null_mut(), security_quality_of_service: std::ptr::null_mut() };
        let mut iosb = NtIoStatusBlock { status: NtIoStatusValue { status: 0 }, information: 0 };
        let mut handle = HANDLE::default();
        let type_option = match directory {
            Some(true) => NT_FILE_DIRECTORY_FILE,
            Some(false) => NT_FILE_NON_DIRECTORY_FILE,
            None => 0,
        };
        let options = NT_FILE_OPEN_REPARSE_POINT | NT_FILE_SYNCHRONOUS_IO_NONALERT | type_option;
        let status = unsafe { NtCreateFile(&mut handle, access,
            &mut attributes, &mut iosb, std::ptr::null(), 0,
            (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0, NT_FILE_OPEN, options,
            std::ptr::null_mut(), 0) };
        let io_status = unsafe { iosb.status.status };
        if status != 0 || io_status != 0 {
            if status == STATUS_OBJECT_NAME_NOT_FOUND {
                return Ok(None);
            }
            let code = if status < 0 { unsafe { RtlNtStatusToDosError(status) } } else { status as u32 };
            return Err(RenameError::Io(format!("NtCreateFile {operation} failed: NTSTATUS 0x{:08x}, error {code}", status as u32)));
        }
        Ok(Some(Handle(handle)))
    }

    pub(super) fn rename_relative_handle_no_replace(source: *mut c_void, destination_parent: *mut c_void, name: &std::ffi::OsStr) -> Result<(), RenameError> {
        use std::os::windows::ffi::OsStrExt;
        native_rename_relative_no_replace_wide(
            HANDLE(source), HANDLE(destination_parent), name.encode_wide().collect(),
        )
    }

    fn native_rename_relative_no_replace(source: HANDLE, destination_parent: HANDLE, name: &str) -> Result<(), RenameError> {
        native_rename_relative_no_replace_wide(source, destination_parent, name.encode_utf16().collect())
    }

    fn native_rename_relative_no_replace_wide(source: HANDLE, destination_parent: HANDLE, wide_name: Vec<u16>) -> Result<(), RenameError> {
        if wide_name.is_empty() || wide_name.contains(&0) { return Err(RenameError::InvalidName); }
        let name_bytes = wide_name.len().checked_mul(size_of::<u16>()).ok_or(RenameError::InvalidName)?;
        let name_offset = offset_of!(NativeFileRenameInformation, file_name);
        let length = (name_offset + name_bytes).max(size_of::<NativeFileRenameInformation>());
        let mut storage = vec![0_u64; length.div_ceil(size_of::<u64>())];
        let info = storage.as_mut_ptr().cast::<NativeFileRenameInformation>();
        unsafe {
            (*info).replace_if_exists = 0;
            (*info).root_directory = destination_parent;
            (*info).file_name_length = u32::try_from(name_bytes).map_err(|_| RenameError::InvalidName)?;
            std::ptr::copy_nonoverlapping(wide_name.as_ptr(), (*info).file_name.as_mut_ptr(), wide_name.len());
        }
        let mut iosb = NtIoStatusBlock { status: NtIoStatusValue { status: 0 }, information: 0 };
        let status = unsafe { NtSetInformationFile(source, &mut iosb, info.cast(), u32::try_from(length).map_err(|_| RenameError::InvalidName)?, 10) };
        let io_status = unsafe { iosb.status.status };
        if status == 0 && io_status == 0 { return Ok(()); }
        // A synchronous name collision is an explicit no-replace refusal; NT leaves IOSB untouched on this path.
        if status as u32 == 0xc000_0035 { return Err(RenameError::DestinationExists); }
        if status != io_status || status >= 0 { return Err(RenameError::MutationUncertain(format!("NtSetInformationFile completion uncertain: status 0x{:08x}, IOSB 0x{:08x}", status as u32, io_status as u32))); }
        let code = unsafe { RtlNtStatusToDosError(status) };
        Err(RenameError::Io(format!("NtSetInformationFile failed: NTSTATUS 0x{:08x}, error {code}", status as u32)))
    }

    #[repr(C)]
    struct NativeFileRenameInformation { replace_if_exists: u8, root_directory: HANDLE, file_name_length: u32, file_name: [u16; 1] }

    pub(super) fn validate_same_volume_move(
        source: &Path,
        destination: &Path,
    ) -> Result<(), RenameError> {
        let (source_probe, parent_probe) = directory_copy_probes(source, destination)?;
        if parent_probe.identity.volume != source_probe.identity.volume {
            return Err(RenameError::Unsupported);
        }
        Ok(())
    }

    pub(super) fn validate_directory_copy_target(
        source: &Path,
        destination: &Path,
    ) -> Result<(), RenameError> {
        let (source_probe, _) = directory_copy_probes(source, destination)?;
        if !source_probe.is_directory {
            return Err(RenameError::Unsupported);
        }
        Ok(())
    }

    fn directory_copy_probes(
        source: &Path,
        destination: &Path,
    ) -> Result<(ObjectProbe, ObjectProbe), RenameError> {
        let source_probe = probe(source)?;
        if source_probe.is_reparse_point {
            return Err(RenameError::Unsupported);
        }
        ensure_non_reparse_ancestors(source)?;

        let destination_parent = destination.parent().ok_or(RenameError::InvalidName)?;
        let parent_probe = probe(destination_parent)?;
        if parent_probe.is_reparse_point || !parent_probe.is_directory {
            return Err(RenameError::Unsupported);
        }

        // Every opened ancestor is checked.  If the destination is inside the
        // directory being moved, this catches it even when the path goes via a
        // different spelling of that same directory.
        let mut cursor = Some(destination_parent);
        while let Some(path) = cursor {
            let probe = probe(path)?;
            if probe.is_reparse_point || probe.identity == source_probe.identity {
                return Err(RenameError::Unsupported);
            }
            cursor = path.parent();
        }
        Ok((source_probe, parent_probe))
    }

    fn ensure_non_reparse_ancestors(path: &Path) -> Result<(), RenameError> {
        let mut cursor = Some(path);
        while let Some(path) = cursor {
            if probe(path)?.is_reparse_point {
                return Err(RenameError::Unsupported);
            }
            cursor = path.parent();
        }
        Ok(())
    }

    fn probe(path: &Path) -> Result<ObjectProbe, RenameError> {
        let handle = open(
            path,
            0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(handle.0, &mut info).map_err(io)?;
        }
        Ok(object_probe(info))
    }

    #[cfg(test)]
    mod directory_copy_tests {
        use super::*;

        #[test]
        fn rejects_a_target_below_its_source_before_staging() {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            std::fs::create_dir(&source).unwrap();
            let child = source.join("child");
            std::fs::create_dir(&child).unwrap();
            let marker = source.join("keep.txt");
            std::fs::write(&marker, b"contents").unwrap();

            let before_identity = probe(&source).unwrap().identity;
            let destination = child.join("copy-of-source");
            assert!(!destination.exists());

            assert_eq!(
                super::super::validate_directory_copy_target(&source, &destination),
                Err(RenameError::Unsupported)
            );

            assert!(probe(&source).unwrap().identity == before_identity);
            assert_eq!(std::fs::read(&marker).unwrap(), b"contents");
            assert!(!destination.exists());
            assert!(std::fs::read_dir(&child).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".gpui-filer-copy-")));
        }

        #[test]
        fn rejects_a_target_below_its_source_through_a_junction_alias() {
            let root = tempfile::Builder::new()
                .prefix("gpui-filer-directory-copy-junction-")
                .tempdir_in(std::env::current_dir().unwrap())
                .unwrap();
            let source = root.path().join("source");
            std::fs::create_dir(&source).unwrap();
            let child = source.join("child");
            std::fs::create_dir(&child).unwrap();
            let marker = source.join("keep.txt");
            std::fs::write(&marker, b"contents").unwrap();

            let junction = root.path().join("junction");
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&junction)
                .arg(&source)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "mklink /J must create the junction fixture"
            );

            let inspect_junction = || {
                let handle = open(
                    &junction,
                    0,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                )
                .unwrap();
                probe_handle(&handle).unwrap()
            };
            let before_source_identity = probe(&source).unwrap().identity;
            let before_junction = inspect_junction();
            assert_eq!(before_junction.reparse_tag, 0xA000_0003);
            let destination_parent = junction.join("child");
            let destination = destination_parent.join("copy-of-source");
            assert!(!destination.starts_with(&source));
            assert!(!destination.exists());

            assert_eq!(
                super::super::validate_directory_copy_target(&source, &destination),
                Err(RenameError::Unsupported)
            );

            assert!(probe(&source).unwrap().identity == before_source_identity);
            assert_eq!(std::fs::read(&marker).unwrap(), b"contents");
            let after_junction = inspect_junction();
            assert!(after_junction.identity == before_junction.identity);
            assert_eq!(after_junction.reparse_tag, 0xA000_0003);
            assert!(!destination.exists());
            assert!(
                std::fs::read_dir(&destination_parent)
                    .unwrap()
                    .all(|entry| !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".gpui-filer-copy-"))
            );
        }
    }

    fn probe_handle(handle: &Handle) -> Result<ObjectProbe, RenameError> {
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(handle.0, &mut info).map_err(io)?;
        }
        let mut probe = object_probe(info);
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
            .map_err(io)?;
        }
        probe.reparse_tag = tag.ReparseTag;
        Ok(probe)
    }

    fn object_probe(
        info: ::windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
    ) -> ObjectProbe {
        ObjectProbe {
            identity: FileIdentity {
                volume: info.dwVolumeSerialNumber,
                index_high: info.nFileIndexHigh,
                index_low: info.nFileIndexLow,
            },
            is_reparse_point: info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
            is_directory: info.dwFileAttributes
                & ::windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0
                != 0,
            reparse_tag: 0,
        }
    }

    fn open(
        path: &Path,
        access: u32,
        flags: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> Result<Handle, RenameError> {
        open_with_share(path, access, flags, FILE_SHARE_READ | FILE_SHARE_WRITE)
    }

    fn open_with_share(
        path: &Path,
        access: u32,
        flags: FILE_FLAGS_AND_ATTRIBUTES,
        share: FILE_SHARE_MODE,
    ) -> Result<Handle, RenameError> {
        let path = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                share,
                None,
                OPEN_EXISTING,
                flags,
                None,
            )
            .map_err(io)?
        };
        Ok(Handle(handle))
    }

    #[cfg(test)]
    pub(crate) fn create_test_mount_point_junction(link: &Path, target: &Path) -> std::io::Result<()> {
        use ::windows::{Win32::{System::IO::DeviceIoControl,
            Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES}}};
        const FSCTL_SET_REPARSE_POINT: u32 = 589_988;
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        use std::os::windows::ffi::OsStrExt;

        std::fs::create_dir(link)?;
        let held = open_with_share(link, FILE_WRITE_ATTRIBUTES.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .map_err(|error| std::io::Error::other(format!("open test junction: {error:?}")))?;
        let substitute = format!(r"\??\{}", target.display()).encode_utf16().collect::<Vec<_>>();
        let print_name = target.as_os_str().encode_wide().collect::<Vec<_>>();
        let mut path_buffer = substitute.clone();
        path_buffer.push(0);
        path_buffer.extend_from_slice(&print_name);
        path_buffer.push(0);
        let data_length = 8 + path_buffer.len() * 2;
        let mut data = vec![0u8; 8 + data_length];
        data[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        data[4..6].copy_from_slice(&(data_length as u16).to_le_bytes());
        data[10..12].copy_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
        data[12..14].copy_from_slice(&(((substitute.len() + 1) * 2) as u16).to_le_bytes());
        data[14..16].copy_from_slice(&((print_name.len() * 2) as u16).to_le_bytes());
        for (index, unit) in path_buffer.iter().enumerate() {
            let offset = 16 + index * 2;
            data[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let mut returned = 0;
        unsafe { DeviceIoControl(held.0, FSCTL_SET_REPARSE_POINT, Some(data.as_ptr().cast()), data.len() as u32, None, 0, Some(&mut returned), None) }
            .map_err(|error| std::io::Error::other(format!("set test junction: {error}")))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn remove_test_mount_point_junction(link: &Path) -> std::io::Result<()> {
        use ::windows::{Win32::{System::IO::DeviceIoControl,
            Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES}}};
        const FSCTL_DELETE_REPARSE_POINT: u32 = 589_996;
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        let held = open_with_share(link, FILE_WRITE_ATTRIBUTES.0,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .map_err(|error| std::io::Error::other(format!("open test junction: {error:?}")))?;
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        let mut returned = 0;
        unsafe { DeviceIoControl(held.0, FSCTL_DELETE_REPARSE_POINT, Some(data.as_ptr().cast()), data.len() as u32, None, 0, Some(&mut returned), None) }
            .map_err(|error| std::io::Error::other(format!("delete test junction: {error}")))?;
        drop(held);
        std::fs::remove_dir(link)
    }

    fn publish_no_replace(source: HANDLE, destination: &Path) -> Result<(), RenameError> {
        let name = wide(destination);
        let bytes = name.len().saturating_sub(1) * size_of::<u16>();
        let size = offset_of!(FILE_RENAME_INFO, FileName) + name.len() * size_of::<u16>();
        let mut storage = vec![0_u64; size.div_ceil(size_of::<u64>())];
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = BOOLEAN(0);
            (*info).RootDirectory = HANDLE::default();
            (*info).FileNameLength = bytes as u32;
            std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
            SetFileInformationByHandle(source, FileRenameInfo, info.cast::<c_void>(), size as u32)
                .map_err(|error| {
                    // HRESULT_FROM_WIN32(ERROR_FILE_EXISTS / ERROR_ALREADY_EXISTS).
                    let code = error.code().0;
                    if code == 0x8007_0050_u32 as i32 || code == 0x8007_00B7_u32 as i32 {
                        RenameError::DestinationExists
                    } else {
                        io(error)
                    }
                })
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn io(error: impl std::fmt::Display) -> RenameError {
        RenameError::Io(error.to_string())
    }

    #[cfg(test)]
    mod guarded_tests {
        use super::*;
        use std::{fs, sync::atomic::AtomicBool};

        #[test]
        fn guarded_move_holds_destination_ancestry_and_uses_absolute_rename() {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let destination_dir = root.path().join("destination").join("nested");
            fs::create_dir_all(&destination_dir).unwrap();
            let source = root.path().join("source.txt");
            fs::write(&source, b"payload").unwrap();
            let expected_source = crate::directory::EntrySourceIdentity::from_path(
                &source,
                crate::directory::EntryKind::File,
            )
            .unwrap()
            .filesystem_identity();
            let expected_destination = crate::directory::EntrySourceIdentity::from_path(
                &destination_dir,
                crate::directory::EntryKind::Folder,
            )
            .unwrap()
            .filesystem_identity();

            let (held, _, _) =
                open_destination_chain(&destination_dir, &expected_destination).unwrap();
            assert!(
                fs::rename(
                    root.path().join("destination"),
                    root.path().join("destination-moved")
                )
                .is_err()
            );
            drop(held);

            move_same_volume_no_replace_guarded(
                &source,
                &destination_dir.join("source.txt"),
                &expected_source,
                &expected_destination,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(!source.exists());
            assert_eq!(
                fs::read(destination_dir.join("source.txt")).unwrap(),
                b"payload"
            );
        }

        fn identity(path: &Path, kind: crate::directory::EntryKind) -> crate::filesystem_identity::FilesystemIdentity {
            crate::directory::EntrySourceIdentity::from_path(path, kind).unwrap().filesystem_identity()
        }

        #[test]
        fn relative_regular_move_undo_handles_one_character_and_distinct_parents() {
            for (case, leaf, same_parent) in [("one", "x", true), ("distinct", "ordinary.txt", false)] {
                let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
                let source_parent = root.path().join("source");
                let destination_parent = if same_parent { source_parent.clone() } else { root.path().join("destination") };
                fs::create_dir_all(&source_parent).unwrap();
                fs::create_dir_all(&destination_parent).unwrap();
                let source = source_parent.join(leaf);
                let destination = destination_parent.join(if case == "one" { "z" } else { "moved-distinct" });
                fs::write(&source, b"move undo payload").unwrap();
                let source_id = identity(&source, crate::directory::EntryKind::File);
                let parent_id = identity(&destination_parent, crate::directory::EntryKind::Folder);
                let receipt = move_same_volume_no_replace_guarded_with_undo(
                    &source, &destination, &source_id, &parent_id, &AtomicBool::new(false), |_| {},
                ).unwrap().expect("native same-volume move should return exact undo proof");
                assert!(!source.exists());
                assert_eq!(identity(&destination, crate::directory::EntryKind::File), source_id);
                undo_move_no_replace(&receipt).unwrap();
                assert_eq!(identity(&source, crate::directory::EntryKind::File), source_id);
                assert!(!destination.exists());
            }
        }

        #[test]
        fn relative_move_undo_advances_keep_both_name_collision_without_replacement() {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let parent = root.path().join("parent");
            fs::create_dir(&parent).unwrap();
            let source = parent.join("x");
            let destination = parent.join("moved");
            fs::write(&source, b"original").unwrap();
            let source_id = identity(&source, crate::directory::EntryKind::File);
            let parent_id = identity(&parent, crate::directory::EntryKind::Folder);
            let receipt = move_same_volume_no_replace_guarded_with_undo(
                &source, &destination, &source_id, &parent_id, &AtomicBool::new(false), |_| {},
            ).unwrap().unwrap();
            fs::write(&source, b"collision sentinel").unwrap();
            fs::write(parent.join("x (2)"), b"second candidate sentinel").unwrap();
            let undo = undo_move_no_replace(&receipt);
            assert!(undo.is_ok(), "unexpected undo result: {undo:?}");
            assert_eq!(fs::read(&source).unwrap(), b"collision sentinel");
            assert_eq!(fs::read(parent.join("x (2)")).unwrap(), b"second candidate sentinel");
            assert_eq!(identity(&parent.join("x (3)"), crate::directory::EntryKind::File), source_id);
            assert!(!destination.exists());
        }

        #[test]
        fn relative_move_undo_inserts_keep_both_suffix_before_extension() {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let parent = root.path().join("parent");
            fs::create_dir(&parent).unwrap();
            let source = parent.join("report.txt");
            let destination = parent.join("moved");
            fs::write(&source, b"original").unwrap();
            let source_id = identity(&source, crate::directory::EntryKind::File);
            let parent_id = identity(&parent, crate::directory::EntryKind::Folder);
            let receipt = move_same_volume_no_replace_guarded_with_undo(
                &source, &destination, &source_id, &parent_id, &AtomicBool::new(false), |_| {},
            ).unwrap().unwrap();
            fs::write(&source, b"collision sentinel").unwrap();
            assert_eq!(undo_move_no_replace(&receipt), Ok(()));
            assert_eq!(fs::read(&source).unwrap(), b"collision sentinel");
            assert_eq!(identity(&parent.join("report (2).txt"), crate::directory::EntryKind::File), source_id);
            assert!(!destination.exists());
        }

        #[test]
        fn relative_directory_move_undo_appends_suffix_after_dotted_name() {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let source_parent = root.path().join("source");
            let destination_parent = root.path().join("destination");
            fs::create_dir(&source_parent).unwrap();
            fs::create_dir(&destination_parent).unwrap();
            let source = source_parent.join("dotted.name");
            let destination = destination_parent.join("moved-folder");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("child.bin"), b"directory payload").unwrap();
            let source_identity = identity(&source, crate::directory::EntryKind::Folder);
            let destination_identity = identity(&destination_parent, crate::directory::EntryKind::Folder);
            let receipt = move_same_volume_no_replace_guarded_with_undo(
                &source, &destination, &source_identity, &destination_identity,
                &std::sync::atomic::AtomicBool::new(false), |_| {},
            ).unwrap().expect("exact ordinary directory Move returns Undo proof");
            assert_eq!(receipt.object_kind, crate::operations::model::MoveUndoObjectKind::Directory);
            assert_eq!(identity(&destination, crate::directory::EntryKind::Folder), source_identity);

            fs::create_dir(&source).unwrap();
            fs::write(source.join("occupant"), b"keep this object").unwrap();
            undo_move_no_replace(&receipt).unwrap();
            let restored = source_parent.join("dotted.name (2)");
            assert_eq!(identity(&restored, crate::directory::EntryKind::Folder), source_identity);
            assert_eq!(fs::read(restored.join("child.bin")).unwrap(), b"directory payload");
            assert_eq!(fs::read(source.join("occupant")).unwrap(), b"keep this object");
            assert!(!destination.exists());
        }

        #[test]
        fn relative_move_undo_candidate_exhaustion_preserves_every_occupant_and_moved_file() {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let parent = root.path().join("parent");
            fs::create_dir(&parent).unwrap();
            let source = parent.join("x");
            let destination = parent.join("moved");
            let moved_bytes = b"moved object";
            let original_bytes = b"original occupant";
            fs::write(&source, moved_bytes).unwrap();
            let source_id = identity(&source, crate::directory::EntryKind::File);
            let parent_id = identity(&parent, crate::directory::EntryKind::Folder);
            let receipt = move_same_volume_no_replace_guarded_with_undo(
                &source, &destination, &source_id, &parent_id, &AtomicBool::new(false), |_| {},
            ).unwrap().unwrap();
            fs::write(&source, original_bytes).unwrap();
            for index in 2..=999 {
                fs::write(parent.join(format!("x ({index})")), index.to_string()).unwrap();
            }

            assert!(matches!(undo_move_no_replace(&receipt), Err(RenameError::DestinationExists)));
            assert_eq!(fs::read(&source).unwrap(), original_bytes);
            for index in 2..=999 {
                assert_eq!(
                    fs::read(parent.join(format!("x ({index})"))).unwrap(),
                    index.to_string().into_bytes()
                );
            }
            assert_eq!(identity(&destination, crate::directory::EntryKind::File), source_id);
            assert_eq!(fs::read(&destination).unwrap(), moved_bytes);
        }

        #[test]
        fn relative_move_after_pinned_parent_becomes_junction_never_targets_junction_destination() {
            use ::windows::{
                Win32::{Foundation::HANDLE, System::IO::DeviceIoControl,
                    Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE}},
            };
            use std::os::windows::ffi::OsStrExt;

            const FSCTL_SET_REPARSE_POINT: u32 = 589_988;
            const FSCTL_DELETE_REPARSE_POINT: u32 = 589_996;
            const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let source_parent = root.path().join("source");
            let destination_parent = root.path().join("destination");
            let external = root.path().join("external-target");
            fs::create_dir_all(&source_parent).unwrap();
            fs::create_dir_all(&destination_parent).unwrap();
            fs::create_dir_all(&external).unwrap();
            let source = source_parent.join("x");
            let destination = destination_parent.join("moved");
            fs::write(&source, b"pinned route payload").unwrap();
            fs::write(external.join("sentinel"), b"must remain unchanged").unwrap();
            let source_id = identity(&source, crate::directory::EntryKind::File);
            let destination_parent_id = identity(&destination_parent, crate::directory::EntryKind::Folder);
            let mut mutation_handle = None;
            let attempt = move_same_volume_no_replace_guarded_with_undo(
                &source, &destination, &source_id, &destination_parent_id, &AtomicBool::new(false),
                |pinned_parent| {
                    let held = open_with_share(&destination_parent, 0x0000_0100,
                        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).unwrap();
                    assert_eq!(probe_handle(&held).unwrap().identity.shared(), destination_parent_id);
                    assert_ne!(pinned_parent, HANDLE::default());
                    install_junction(&held, &external);
                    mutation_handle = Some(held);
                },
            );
            let held = mutation_handle.as_ref().expect("mutation hook ran after route pinning");
            assert_eq!(fs::read(external.join("sentinel")).unwrap(), b"must remain unchanged");
            assert!(!external.join("moved").exists(), "RootDirectory rename must not traverse the installed junction");
            let pinned_leaf = nt_open_relative(held, std::ffi::OsStr::new("moved"), Some(false));
            let moved_under_pinned_parent = pinned_leaf.as_ref().is_ok_and(|file| probe_handle(file).is_ok_and(|probe| probe.identity.shared() == source_id));
            assert!(moved_under_pinned_parent || source.exists(),
                "mutation either stays under the pinned directory or refuses before moving the source: {attempt:?}");
            remove_junction(held);
            drop(mutation_handle);
            if let Ok(Some(receipt)) = attempt {
                assert!(moved_under_pinned_parent);
                undo_move_no_replace(&receipt).unwrap();
            } else if moved_under_pinned_parent {
                let receipt = crate::operations::model::MoveUndoReceipt {
                    source: source.clone(), destination: destination.clone(), object_identity: source_id,
                    object_kind: crate::operations::model::MoveUndoObjectKind::RegularFile,
                    source_parent_route: route_proof(&open_relative_route(&source_parent).unwrap()),
                    destination_parent_route: route_proof(&open_relative_route(&destination_parent).unwrap()),
                    cross_volume_file: None,
                };
                undo_move_no_replace(&receipt).unwrap();
            }
            assert_eq!(fs::read(&source).unwrap(), b"pinned route payload");

            fn install_junction(handle: &Handle, target: &Path) {
                let substitute = format!(r"\??\{}", target.display()).encode_utf16().collect::<Vec<_>>();
                let print_name = target.as_os_str().encode_wide().collect::<Vec<_>>();
                let mut path_buffer = substitute.clone(); path_buffer.push(0); path_buffer.extend_from_slice(&print_name); path_buffer.push(0);
                let data_length = 8 + path_buffer.len() * 2;
                let mut data = vec![0u8; 8 + data_length];
                data[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
                data[4..6].copy_from_slice(&(data_length as u16).to_le_bytes());
                data[10..12].copy_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
                data[12..14].copy_from_slice(&(((substitute.len() + 1) * 2) as u16).to_le_bytes());
                data[14..16].copy_from_slice(&((print_name.len() * 2) as u16).to_le_bytes());
                for (index, unit) in path_buffer.iter().enumerate() { let offset = 16 + index * 2; data[offset..offset + 2].copy_from_slice(&unit.to_le_bytes()); }
                let mut returned = 0;
                unsafe { DeviceIoControl(handle.0, FSCTL_SET_REPARSE_POINT, Some(data.as_ptr().cast()), data.len() as u32, None, 0, Some(&mut returned), None) }.unwrap();
            }
            fn remove_junction(handle: &Handle) {
                let mut data = [0u8; 8]; data[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
                let mut returned = 0;
                unsafe { DeviceIoControl(handle.0, FSCTL_DELETE_REPARSE_POINT, Some(data.as_ptr().cast()), data.len() as u32, None, 0, Some(&mut returned), None) }.unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_like_or_empty_names() {
        for name in ["", ".", "..", "dir\\name", "dir/name"] {
            assert_eq!(
                rename_in_parent(Path::new("source"), name),
                Err(RenameError::InvalidName)
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn renames_without_replacing_an_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination.txt");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"old").unwrap();
        assert!(rename_in_parent(&source, "destination.txt").is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"new");
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        let receipt = rename_in_parent(&source, "renamed.txt").unwrap().unwrap();
        assert_eq!(receipt.destination, root.path().join("renamed.txt"));
        assert_eq!(
            std::fs::read(root.path().join("renamed.txt")).unwrap(),
            b"new"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn accepted_file_directory_symlink_and_junction_renames_preserve_the_link_object() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let root = tempfile::tempdir().unwrap();
        let file_target = root.path().join("file-target.txt");
        std::fs::write(&file_target, b"file target contents").unwrap();
        let file_link = root.path().join("file-link");
        if symlink_file(&file_target, &file_link).is_err() {
            return;
        }
        let file_raw_target = std::fs::read_link(&file_link).unwrap();
        let file_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &file_link,
            crate::directory::EntryKind::File,
        )
        .unwrap()
        .filesystem_identity();
        let file_guard = probe_reparse_source(
            &file_link,
            &file_identity,
            &crate::directory::EntrySourceIdentity::from_path(
                root.path(),
                crate::directory::EntryKind::Folder,
            )
            .unwrap()
            .filesystem_identity(),
        )
        .unwrap();
        assert_eq!(file_guard.reparse_tag, 0xA000_000C);
        let renamed_file_link = root.path().join("file-link-renamed");
        rename_reparse_sibling_guarded(
            &file_link,
            &renamed_file_link,
            &file_guard,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(&renamed_file_link).unwrap(),
            file_raw_target
        );
        assert_eq!(
            std::fs::read(&file_target).unwrap(),
            b"file target contents"
        );

        let directory_target = root.path().join("directory-target");
        std::fs::create_dir(&directory_target).unwrap();
        std::fs::write(directory_target.join("inside.txt"), b"directory target").unwrap();
        let directory_link = root.path().join("directory-link");
        if symlink_dir(&directory_target, &directory_link).is_err() {
            return;
        }
        let directory_raw_target = std::fs::read_link(&directory_link).unwrap();
        let parent_identity = crate::directory::EntrySourceIdentity::from_path(
            root.path(),
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        let directory_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &directory_link,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        let directory_guard =
            probe_reparse_source(&directory_link, &directory_identity, &parent_identity).unwrap();
        assert_eq!(directory_guard.reparse_tag, 0xA000_000C);
        let renamed_directory_link = root.path().join("directory-link-renamed");
        rename_reparse_sibling_guarded(
            &directory_link,
            &renamed_directory_link,
            &directory_guard,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(&renamed_directory_link).unwrap(),
            directory_raw_target
        );
        assert_eq!(
            std::fs::read(directory_target.join("inside.txt")).unwrap(),
            b"directory target"
        );

        let junction_target = root.path().join("junction-target");
        std::fs::create_dir(&junction_target).unwrap();
        std::fs::write(junction_target.join("inside.txt"), b"junction target").unwrap();
        let junction = root.path().join("junction");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&junction_target)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "mklink /J must create the junction fixture"
        );
        let junction_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &junction,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        let junction_guard =
            probe_reparse_source(&junction, &junction_identity, &parent_identity).unwrap();
        assert_eq!(junction_guard.reparse_tag, 0xA000_0003);
        let renamed_junction = root.path().join("junction-renamed");
        rename_reparse_sibling_guarded(
            &junction,
            &renamed_junction,
            &junction_guard,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(renamed_junction.exists());
        assert_eq!(
            std::fs::read(junction_target.join("inside.txt")).unwrap(),
            b"junction target"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reparse_rename_guard_fails_closed_for_replacement_parent_collision_and_unknown_tag() {
        use std::os::windows::fs::symlink_file;

        let root = tempfile::tempdir().unwrap();
        let first_target = root.path().join("first-target");
        let second_target = root.path().join("second-target");
        std::fs::write(&first_target, b"first").unwrap();
        std::fs::write(&second_target, b"second").unwrap();
        let source = root.path().join("source-link");
        if symlink_file(&first_target, &source).is_err() {
            return;
        }
        let parent_identity = crate::directory::EntrySourceIdentity::from_path(
            root.path(),
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        let accepted_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &source,
            crate::directory::EntryKind::File,
        )
        .unwrap()
        .filesystem_identity();
        let accepted = probe_reparse_source(&source, &accepted_identity, &parent_identity).unwrap();
        let displaced = root.path().join("displaced-link");
        std::fs::rename(&source, &displaced).unwrap();
        symlink_file(&second_target, &source).unwrap();
        let destination = root.path().join("renamed-link");
        assert_eq!(
            rename_reparse_sibling_guarded(
                &source,
                &destination,
                &accepted,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::IdentityMismatch)
        );
        assert!(source.exists());
        assert!(!destination.exists());

        let current_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &source,
            crate::directory::EntryKind::File,
        )
        .unwrap()
        .filesystem_identity();
        let current = probe_reparse_source(&source, &current_identity, &parent_identity).unwrap();
        let occupied = root.path().join("occupied");
        std::fs::write(&occupied, b"keep").unwrap();
        assert_eq!(
            rename_reparse_sibling_guarded(
                &source,
                &occupied,
                &current,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::DestinationExists)
        );
        assert_eq!(std::fs::read(&occupied).unwrap(), b"keep");

        let wrong_parent =
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(0, u64::MAX);
        assert!(matches!(
            probe_reparse_source(&source, &current_identity, &wrong_parent),
            Err(RenameError::IdentityMismatch | RenameError::ReparsePoint)
        ));

        let replacement_parent = root.path().join("replacement-parent");
        std::fs::create_dir(&replacement_parent).unwrap();
        let replacement_source = replacement_parent.join("source-link");
        symlink_file(&second_target, &replacement_source).unwrap();
        assert!(matches!(
            rename_reparse_sibling_guarded(
                &replacement_source,
                &replacement_parent.join("renamed-link"),
                &current,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::IdentityMismatch)
        ));

        let mut unknown = current.clone();
        unknown.reparse_tag = 0xDEAD_BEEF;
        assert_eq!(
            rename_reparse_sibling_guarded(
                &source,
                &root.path().join("unknown-tag-result"),
                &unknown,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::ReparsePoint)
        );
        assert_eq!(
            rename_reparse_sibling_guarded(
                &source,
                &root.path().join("SOURCE-LINK"),
                &current,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::ReparsePoint)
        );
        assert_eq!(std::fs::read(&first_target).unwrap(), b"first");
        assert_eq!(std::fs::read(&second_target).unwrap(), b"second");

        let ordinary = root.path().join("ordinary.txt");
        std::fs::write(&ordinary, b"ordinary").unwrap();
        assert_eq!(
            probe_reparse_source(
                &ordinary,
                &crate::directory::EntrySourceIdentity::from_path_no_follow(
                    &ordinary,
                    crate::directory::EntryKind::File,
                )
                .unwrap()
                .filesystem_identity(),
                &parent_identity,
            ),
            Err(RenameError::ReparsePoint)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn same_volume_move_is_a_no_replace_handle_rename() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("from");
        let to = root.path().join("to");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let source = from.join("source.txt");
        let destination = to.join("source.txt");
        std::fs::write(&source, b"new").unwrap();
        let receipt = move_same_volume_no_replace(&source, &destination)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.destination, destination);
        assert!(!source.exists());
        assert_eq!(std::fs::read(&receipt.destination).unwrap(), b"new");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn guarded_directory_move_rejects_its_opened_descendant_target() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = source.join("nested");
        std::fs::create_dir_all(&destination).unwrap();
        let source_identity = crate::directory::EntrySourceIdentity::from_path(
            &source,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        let destination_identity = crate::directory::EntrySourceIdentity::from_path(
            &destination,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();

        assert_eq!(
            move_same_volume_no_replace_guarded(
                &source,
                &destination.join("source"),
                &source_identity,
                &destination_identity,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::Unsupported),
        );
        assert!(source.join("nested").is_dir());
        assert!(!destination.join("source").exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn guarded_move_reports_destination_name_collision_distinctly() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("source.txt");
        let destination_dir = root.path().join("destination");
        std::fs::create_dir(&destination_dir).unwrap();
        let destination = destination_dir.join("source.txt");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&destination, b"destination").unwrap();
        let source_identity = crate::directory::EntrySourceIdentity::from_path(
            &source,
            crate::directory::EntryKind::File,
        )
        .unwrap()
        .filesystem_identity();
        let destination_identity = crate::directory::EntrySourceIdentity::from_path(
            &destination_dir,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();

        assert_eq!(
            move_same_volume_no_replace_guarded(
                &source,
                &destination,
                &source_identity,
                &destination_identity,
                &std::sync::atomic::AtomicBool::new(false),
            ),
            Err(RenameError::DestinationExists),
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert_eq!(std::fs::read(&destination).unwrap(), b"destination");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn same_volume_move_moves_a_nonempty_directory_without_traversing_it() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("from");
        let to = root.path().join("to");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let source = from.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("child.txt"), b"contents").unwrap();

        let destination = to.join("source");
        let receipt = move_same_volume_no_replace(&source, &destination)
            .unwrap()
            .unwrap();

        assert_eq!(receipt.destination, destination);
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(receipt.destination.join("child.txt")).unwrap(),
            b"contents"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn same_volume_move_rejects_a_directory_target_below_its_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let child = source.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(source.join("keep.txt"), b"contents").unwrap();

        let destination = child.join("source");
        assert_eq!(
            move_same_volume_no_replace(&source, &destination),
            Err(RenameError::Unsupported)
        );
        assert!(source.exists());
        assert!(!destination.exists());
        assert_eq!(std::fs::read(source.join("keep.txt")).unwrap(), b"contents");
    }
}
