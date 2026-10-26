use super::*;

use super::super::create_shortcut::CreateShortcutResult;
use std::{
    ffi::OsString,
    io::Write,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_LINK_BYTES: u64 = 1024 * 1024;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SHORTCUT_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_NEXT_SHORTCUT_FLUSH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CANCEL_AFTER_SHORTCUT_SERIALIZE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CANCEL_AFTER_SHORTCUT_CREATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(in crate::operations::executor) fn create_shortcut(
    source: &Path,
    source_kind: crate::directory::EntryKind,
    source_identity: &crate::filesystem_identity::FilesystemIdentity,
    parent_identity: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
) -> CreateShortcutResult {
    match create_shortcut_inner(
        source,
        source_kind,
        source_identity,
        parent_identity,
        cancelled,
    ) {
        Ok(result) => result,
        Err(error) => CreateShortcutResult::Failed(error),
    }
}

fn create_shortcut_inner(
    source: &Path,
    source_kind: crate::directory::EntryKind,
    source_identity: &crate::filesystem_identity::FilesystemIdentity,
    parent_identity: &crate::filesystem_identity::FilesystemIdentity,
    cancelled: &AtomicBool,
) -> Result<CreateShortcutResult, CopyError> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(CreateShortcutResult::Cancelled);
    }
    if !matches!(
        source_kind,
        crate::directory::EntryKind::File | crate::directory::EntryKind::Folder
    ) {
        return Err(CopyError::Unsupported);
    }
    let parent_path = source.parent().ok_or(CopyError::Unsupported)?;
    let route = lock_virtual_destination(parent_path, Some(parent_identity))?;
    let parent = route.last().ok_or(CopyError::Unsupported)?;
    let parent_text = directory_identity_text(parent)?;
    let source_name = source.file_name().ok_or(CopyError::Unsupported)?;
    let source_component = source_name.encode_wide().collect::<Vec<_>>();
    let source_handle = nt_open_or_create_child(
        parent,
        &source_component,
        source_kind == crate::directory::EntryKind::Folder,
        false,
        NT_FILE_OPEN,
        FILE_SHARE_READ.0,
        false,
    )?;
    let source_file = if source_kind == crate::directory::EntryKind::File {
        Some(unsafe { std::fs::File::from_raw_handle(source_handle.0) })
    } else {
        None
    };
    let source_directory = if source_kind == crate::directory::EntryKind::Folder {
        Some(DirectoryLock(source_handle))
    } else {
        None
    };
    let actual_source_identity = match (&source_file, &source_directory) {
        (Some(file), None) => {
            let identity = regular_file_identity(file)?;
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                identity.volume,
                (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
            )
        }
        (None, Some(directory)) => {
            let identity = directory_identity(directory)?;
            crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                identity.volume,
                (u64::from(identity.index_high) << 32) | u64::from(identity.index_low),
            )
        }
        _ => return Err(CopyError::Unsupported),
    };
    if &actual_source_identity != source_identity {
        return Err(CopyError::GuardRejected(
            "shortcut source identity changed".into(),
        ));
    }

    // 最終名を作る前に直列化を終え、出力を開いたままにする時間を短くする。
    let payload = serialize_link(source, parent_path)?;
    #[cfg(test)]
    if CANCEL_AFTER_SHORTCUT_SERIALIZE.with(|fault| fault.replace(false)) {
        cancelled.store(true, Ordering::Release);
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(CreateShortcutResult::Cancelled);
    }

    let mut leaf = OsString::from(source_name);
    leaf.push(".lnk");
    let component = leaf.encode_wide().collect::<Vec<_>>();
    let destination = parent_path.join(&leaf);
    let raw = nt_open_or_create_child(parent, &component, false, true, NT_FILE_CREATE, 0, true)?;
    let output = unsafe { std::fs::File::from_raw_handle(raw.0) };
    #[cfg(test)]
    if CANCEL_AFTER_SHORTCUT_CREATE.with(|fault| fault.replace(false)) {
        cancelled.store(true, Ordering::Release);
    }
    let write_result = (|| {
        #[cfg(test)]
        if FAIL_NEXT_SHORTCUT_WRITE.with(|fault| fault.replace(false)) {
            return Err(CopyError::Io("injected shortcut write failure".into()));
        }
        let mut writer = &output;
        writer.write_all(&payload).map_err(io)?;
        #[cfg(test)]
        if FAIL_NEXT_SHORTCUT_FLUSH.with(|fault| fault.replace(false)) {
            return Err(CopyError::Io("injected shortcut flush failure".into()));
        }
        output.sync_all().map_err(io)?;
        Ok::<(), CopyError>(())
    })();
    if let Err(error) = write_result {
        return match cleanup_owned(&output) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Ok(CreateShortcutResult::Retained {
                destination,
                error: cleanup_error,
            }),
        };
    }

    let size = payload.len() as u64;
    let undo = if size <= MAX_LINK_BYTES {
        let proof = (|| {
            if output.metadata().map_err(io)?.len() != size
                || parent_text != directory_identity_text(parent)?
            {
                return Err(CopyError::Unsupported);
            }
            let identity = regular_file_identity(&output)?;
            let signature = leaf_signature(&output)?;
            if signature.number_of_links != 1 || output.metadata().map_err(io)?.len() != size {
                return Err(CopyError::Unsupported);
            }
            let content_sha256 = hash_held_file(&output)?;
            Ok(crate::operations::model::CreatedObjectUndoReceipt {
                destination: destination.clone(),
                parent_identity: parent_text.clone(),
                object_identity: format!(
                    "{}:{:08x}{:08x}",
                    identity.volume, identity.index_high, identity.index_low
                ),
                kind: crate::operations::model::CreatedObjectUndoKind::ShortcutFile {
                    content_sha256,
                    signature,
                    size,
                },
            })
        })();
        proof.ok()
    } else {
        None
    };
    Ok(CreateShortcutResult::Created {
        destination,
        size,
        undo,
    })
}

fn serialize_link(target: &Path, working_directory: &Path) -> Result<Vec<u8>, CopyError> {
    use ::windows::{
        Win32::{
            System::{
                Com::StructuredStorage::CreateStreamOnHGlobal,
                Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoUninitialize, IStream, STATFLAG_NONAME, STREAM_SEEK,
                },
            },
            UI::Shell::{IShellLinkW, ShellLink},
        },
        core::{Interface, PCWSTR},
    };

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(io)?;
    struct Uninit;
    impl Drop for Uninit {
        fn drop(&mut self) {
            unsafe { CoUninitialize() }
        }
    }
    let _uninit = Uninit;
    let link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.map_err(io)?;
    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let working_wide = working_directory
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe { link.SetPath(PCWSTR(target_wide.as_ptr())) }.map_err(io)?;
    unsafe { link.SetWorkingDirectory(PCWSTR(working_wide.as_ptr())) }.map_err(io)?;
    let persist: ::windows::Win32::System::Com::IPersistStream = link.cast().map_err(io)?;
    let stream: IStream = unsafe { CreateStreamOnHGlobal(None, true) }.map_err(io)?;
    let maximum = unsafe { persist.GetSizeMax() }.map_err(io)?;
    if maximum == 0 || maximum > MAX_LINK_BYTES {
        return Err(CopyError::Unsupported);
    }
    unsafe { persist.Save(&stream, true) }.map_err(io)?;
    let mut stat = ::windows::Win32::System::Com::STATSTG::default();
    unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.map_err(io)?;
    let length = stat.cbSize;
    if length == 0 || length > MAX_LINK_BYTES || length > maximum {
        return Err(CopyError::Unsupported);
    }
    unsafe { stream.Seek(0, STREAM_SEEK(0), None) }.map_err(io)?;
    let mut bytes = vec![0u8; usize::try_from(length).map_err(io)?];
    let mut read = 0u32;
    unsafe {
        stream.Read(
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            Some(&mut read),
        )
    }
    .ok()
    .map_err(io)?;
    if read as usize != bytes.len() {
        return Err(CopyError::Unsupported);
    }
    drop(stream);
    drop(persist);
    drop(link);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    fn loaded_target(path: &Path) -> PathBuf {
        use ::windows::{
            Win32::{
                System::Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoUninitialize, IPersistFile, STGM_READ,
                },
                UI::Shell::{IShellLinkW, ShellLink},
            },
            core::{Interface, PCWSTR},
        };
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        struct Uninit;
        impl Drop for Uninit {
            fn drop(&mut self) {
                unsafe { CoUninitialize() }
            }
        }
        let _uninit = Uninit;
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.unwrap();
        let persist: IPersistFile = link.cast().unwrap();
        let path_wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        unsafe { persist.Load(PCWSTR(path_wide.as_ptr()), STGM_READ) }.unwrap();
        let mut target = vec![0u16; 32768];
        unsafe { link.GetPath(&mut target, std::ptr::null_mut(), 0) }.unwrap();
        let length = target.iter().position(|unit| *unit == 0).unwrap();
        PathBuf::from(OsString::from_wide(&target[..length]))
    }

    fn identity(
        path: &Path,
        kind: crate::directory::EntryKind,
    ) -> crate::filesystem_identity::FilesystemIdentity {
        crate::directory::EntrySourceIdentity::from_path(path, kind)
            .expect("fixture identity")
            .filesystem_identity()
    }

    #[test]
    fn shortcut_file_creation_is_unicode_and_collision_safe() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("資料.txt");
        std::fs::write(&source, b"source bytes").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(root.path(), crate::directory::EntryKind::Folder);
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::File,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        let CreateShortcutResult::Created {
            destination,
            size,
            undo,
        } = result
        else {
            panic!("shortcut creation failed")
        };
        assert_eq!(destination.file_name().unwrap(), OsStr::new("資料.txt.lnk"));
        assert!(size > 0 && size <= MAX_LINK_BYTES);
        assert_eq!(loaded_target(&destination), source);
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        let undo = undo.expect("proof for unchanged ShellLink");
        assert!(super::super::super::undo_created_object(undo).is_ok());
        assert!(!destination.exists());
        std::fs::write(&destination, b"sentinel").unwrap();
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::File,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        assert!(matches!(
            result,
            CreateShortcutResult::Failed(CopyError::DestinationExists)
        ));
        assert_eq!(std::fs::read(destination).unwrap(), b"sentinel");
    }

    #[test]
    fn cancellation_precedes_create_and_stale_guard_is_rejected() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("file.txt");
        std::fs::write(&source, b"x").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(root.path(), crate::directory::EntryKind::Folder);
        assert!(matches!(
            create_shortcut(
                &source,
                crate::directory::EntryKind::File,
                &source_id,
                &parent_id,
                &AtomicBool::new(true)
            ),
            CreateShortcutResult::Cancelled
        ));
        assert!(!root.path().join("file.txt.lnk").exists());
        let replacement_id =
            crate::filesystem_identity::FilesystemIdentity::from_fallback_token("stale".into());
        assert!(matches!(
            create_shortcut(
                &source,
                crate::directory::EntryKind::File,
                &replacement_id,
                &parent_id,
                &AtomicBool::new(false)
            ),
            CreateShortcutResult::Failed(_)
        ));
        assert!(!root.path().join("file.txt.lnk").exists());
    }

    #[test]
    fn cancellation_after_serialization_has_no_effect_and_after_create_keeps_success() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("file.txt");
        std::fs::write(&source, b"source bytes").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(root.path(), crate::directory::EntryKind::Folder);
        let cancelled = AtomicBool::new(false);

        CANCEL_AFTER_SHORTCUT_SERIALIZE.with(|fault| fault.set(true));
        assert!(matches!(
            create_shortcut(
                &source,
                crate::directory::EntryKind::File,
                &source_id,
                &parent_id,
                &cancelled
            ),
            CreateShortcutResult::Cancelled
        ));
        assert!(!root.path().join("file.txt.lnk").exists());

        let cancelled = AtomicBool::new(false);
        CANCEL_AFTER_SHORTCUT_CREATE.with(|fault| fault.set(true));
        assert!(matches!(
            create_shortcut(
                &source,
                crate::directory::EntryKind::File,
                &source_id,
                &parent_id,
                &cancelled
            ),
            CreateShortcutResult::Created { .. }
        ));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(root.path().join("file.txt.lnk").exists());
    }

    #[test]
    fn parent_reuse_and_reparse_source_are_rejected() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("file.txt");
        std::fs::write(&source, b"x").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(&parent, crate::directory::EntryKind::Folder);
        let moved_parent = root.path().join("old-parent");
        std::fs::rename(&parent, &moved_parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("file.txt"), b"replacement").unwrap();
        assert!(matches!(
            create_shortcut(
                &source,
                crate::directory::EntryKind::File,
                &source_id,
                &parent_id,
                &AtomicBool::new(false)
            ),
            CreateShortcutResult::Failed(_)
        ));
        assert!(!parent.join("file.txt.lnk").exists());

        let target = root.path().join("target.txt");
        let link = root.path().join("link.txt");
        std::fs::write(&target, b"target").unwrap();
        if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
            let link_identity = identity(&link, crate::directory::EntryKind::File);
            let root_id = identity(root.path(), crate::directory::EntryKind::Folder);
            assert!(matches!(
                create_shortcut(
                    &link,
                    crate::directory::EntryKind::File,
                    &link_identity,
                    &root_id,
                    &AtomicBool::new(false)
                ),
                CreateShortcutResult::Failed(_)
            ));
            assert!(!root.path().join("link.txt.lnk").exists());
        } else {
            eprintln!("reparse source fixture unavailable; reparse rejection was not exercised");
        }
    }

    #[test]
    fn folder_target_and_exact_undo_are_supported() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("資料フォルダー");
        std::fs::create_dir(&source).unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::Folder);
        let parent_id = identity(root.path(), crate::directory::EntryKind::Folder);
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::Folder,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        let CreateShortcutResult::Created {
            destination,
            undo: Some(undo),
            ..
        } = result
        else {
            panic!("folder shortcut creation failed")
        };
        assert_eq!(loaded_target(&destination), source);
        assert!(super::super::super::undo_created_object(undo).is_ok());
        assert!(!destination.exists());
        assert!(source.is_dir());
    }

    #[test]
    fn modified_or_hardlinked_shortcuts_do_not_get_removed() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("file.txt");
        std::fs::write(&source, b"x").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(root.path(), crate::directory::EntryKind::Folder);
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::File,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        let CreateShortcutResult::Created {
            destination,
            undo: Some(undo),
            ..
        } = result
        else {
            panic!("shortcut creation failed")
        };
        std::fs::OpenOptions::new()
            .append(true)
            .open(&destination)
            .unwrap()
            .write_all(b"edit")
            .unwrap();
        assert!(super::super::super::undo_created_object(undo).is_err());
        assert!(destination.exists());

        let second = root.path().join("other.txt");
        std::fs::write(&second, b"y").unwrap();
        let second_id = identity(&second, crate::directory::EntryKind::File);
        let result = create_shortcut(
            &second,
            crate::directory::EntryKind::File,
            &second_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        let CreateShortcutResult::Created {
            destination, undo, ..
        } = result
        else {
            panic!("shortcut creation failed")
        };
        assert!(undo.is_some());
        let hardlink = root.path().join("hardlink.lnk");
        std::fs::hard_link(&destination, &hardlink).unwrap();
        let receipt = undo.unwrap();
        assert!(super::super::super::undo_created_object(receipt).is_err());
        assert!(destination.exists() && hardlink.exists());
    }

    #[test]
    fn undo_preserves_replaced_parent_and_destination() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("file.txt");
        std::fs::write(&source, b"x").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(&parent, crate::directory::EntryKind::Folder);
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::File,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        let CreateShortcutResult::Created {
            destination,
            undo: Some(undo),
            ..
        } = result
        else {
            panic!("shortcut creation failed")
        };
        let old_parent = root.path().join("old-parent");
        std::fs::rename(&parent, &old_parent).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(&source, b"replacement source").unwrap();
        std::fs::write(&destination, b"replacement shortcut").unwrap();
        assert!(super::super::super::undo_created_object(undo).is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"replacement shortcut");
        assert!(old_parent.join("file.txt.lnk").exists());
    }

    #[test]
    fn write_failure_is_cleaned_or_reported_as_retained() {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let source = root.path().join("file.txt");
        std::fs::write(&source, b"x").unwrap();
        let source_id = identity(&source, crate::directory::EntryKind::File);
        let parent_id = identity(root.path(), crate::directory::EntryKind::Folder);
        FAIL_NEXT_SHORTCUT_WRITE.with(|fault| fault.set(true));
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::File,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        assert!(matches!(
            result,
            CreateShortcutResult::Failed(CopyError::Io(_))
        ));
        assert!(!root.path().join("file.txt.lnk").exists());

        FAIL_NEXT_SHORTCUT_FLUSH.with(|fault| fault.set(true));
        let result = create_shortcut(
            &source,
            crate::directory::EntryKind::File,
            &source_id,
            &parent_id,
            &AtomicBool::new(false),
        );
        assert!(matches!(
            result,
            CreateShortcutResult::Failed(CopyError::Io(_))
        ));
        assert!(!root.path().join("file.txt.lnk").exists());

        FAIL_NEXT_SHORTCUT_WRITE.with(|fault| fault.set(true));
        let (result, _) = super::super::with_copy_test_faults_for_test(true, false, || {
            create_shortcut(
                &source,
                crate::directory::EntryKind::File,
                &source_id,
                &parent_id,
                &AtomicBool::new(false),
            )
        });
        assert!(matches!(result, CreateShortcutResult::Retained { .. }));
    }
}
