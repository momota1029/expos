//! Immutable process-session admission fact for the application helper image.

use super::{ApplicationRouteLease, ArchiveFileHint, ArchiveFileIdentity, lease_application_route};
use std::{
    fs::File,
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::{Path, PathBuf},
    sync::OnceLock,
};
use windows::Win32::{
    Foundation::{GENERIC_READ, HANDLE},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        GetFileInformationByHandle, GetVolumeInformationByHandleW, ReOpenFile,
    },
};

static CAPTURED: OnceLock<Option<CapturedApplicationImage>> = OnceLock::new();

struct CapturedApplicationImage {
    path: PathBuf,
    identity: ArchiveFileIdentity,
    _leaf: File,
}

/// Operation-scoped no-follow application route, held until the helper Job drains.
pub(crate) struct ApplicationImageLease {
    root: ArchiveFileHint,
    _route: ApplicationRouteLease,
}

impl ApplicationImageLease {
    pub(crate) fn expected_root(&self) -> &ArchiveFileHint {
        &self.root
    }
}

impl CapturedApplicationImage {
    fn lease(&self) -> Option<ApplicationImageLease> {
        let route = lease_application_route(&self.path)?;
        if route.identity() != self.identity {
            return None;
        }
        Some(ApplicationImageLease {
            root: ArchiveFileHint {
                path: self.path.clone(),
                identity: self.identity,
            },
            _route: route,
        })
    }
}

/// Captures a missing-or-present startup fact once; failure never disables GUI startup.
pub(crate) fn initialize() {
    let _ = initialize_in(&CAPTURED, capture_current_image);
}

pub(crate) fn lease_for_operation() -> Option<ApplicationImageLease> {
    CAPTURED.get()?.as_ref()?.lease()
}

/// 統合 fixture が指定した実行ファイルを個別に lease する。production capture には触れない。
#[cfg(test)]
pub(crate) fn lease_explicit_path_for_test(path: &Path) -> Option<ApplicationImageLease> {
    capture_path(path)?.lease()
}

fn initialize_in<'a>(
    slot: &'a OnceLock<Option<CapturedApplicationImage>>,
    capture: impl FnOnce() -> Option<CapturedApplicationImage>,
) -> &'a Option<CapturedApplicationImage> {
    slot.get_or_init(capture)
}

fn capture_current_image() -> Option<CapturedApplicationImage> {
    let path = std::env::current_exe().ok()?;
    capture_path(&path)
}

fn capture_path(path: &Path) -> Option<CapturedApplicationImage> {
    if !path.is_absolute() || path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return None;
    }

    let route = lease_application_route(path)?;
    let original = route.leaf_handle();
    if original.is_invalid() {
        return None;
    }
    if !is_ntfs(original) {
        return None;
    }
    let identity = route.identity();

    // 既存の leaf handle から reopen し、祖先 route を保持せず leaf だけを残す。
    // DELETE share により更新時の rename は許し、WRITE share は与えない。
    let reopened = unsafe {
        ReOpenFile(
            original,
            GENERIC_READ.0 | FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .ok()?
    };
    // SAFETY: ReOpenFile が返した新規所有 handle を File に一度だけ移す。
    let leaf = unsafe { File::from_raw_handle(reopened.0 as *mut _) };
    let reopened = HANDLE(leaf.as_raw_handle().cast());
    let reopened_identity = identity_from_handle(reopened)?;
    if reopened_identity != identity || !ordinary_file(reopened) || !is_ntfs(reopened) {
        return None;
    }
    drop(route);
    Some(CapturedApplicationImage {
        path: path.to_owned(),
        identity,
        _leaf: leaf,
    })
}

fn identity_from_handle(handle: HANDLE) -> Option<ArchiveFileIdentity> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info).ok()? };
    Some(ArchiveFileIdentity {
        volume_serial: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

fn ordinary_file(handle: HANDLE) -> bool {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut info) }.is_err() {
        return false;
    }
    info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) == 0
}

fn is_ntfs(handle: HANDLE) -> bool {
    let mut filesystem = [0u16; 64];
    unsafe { GetVolumeInformationByHandleW(handle, None, None, None, None, Some(&mut filesystem)) }
        .is_ok()
        && String::from_utf16_lossy(&filesystem)
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("NTFS")
}

use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn temp_image() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("application.exe");
        fs::write(&path, b"application image fixture").unwrap();
        (directory, path)
    }

    #[test]
    fn one_time_slot_never_recaptures_and_stores_failure() {
        let (_directory, path) = temp_image();
        let slot = OnceLock::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let first_calls = calls.clone();
        initialize_in(&slot, || {
            first_calls.fetch_add(1, Ordering::SeqCst);
            capture_path(&path)
        });
        initialize_in(&slot, || {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let failed: OnceLock<Option<CapturedApplicationImage>> = OnceLock::new();
        initialize_in(&failed, || None);
        initialize_in(&failed, || {
            panic!("a failed session capture must stay unavailable")
        });
        assert!(failed.get().unwrap().is_none());
    }

    #[test]
    fn capture_rejects_missing_relative_nul_and_directory_routes() {
        let (directory, file) = temp_image();
        assert!(capture_path(&directory.path().join("missing.exe")).is_none());
        assert!(capture_path(Path::new("relative.exe")).is_none());
        assert!(capture_path(directory.path()).is_none());

        let with_nul = PathBuf::from(format!("{}\0", file.display()));
        assert!(capture_path(&with_nul).is_none());

        let link = directory.path().join("application-link.exe");
        if std::os::windows::fs::symlink_file(&file, &link).is_ok() {
            assert!(capture_path(&link).is_none());
        }
    }

    #[test]
    fn startup_capture_pins_only_leaf_and_allows_delete_share() {
        let (directory, _) = temp_image();
        let parent = directory.path().join("application-parent");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("application.exe");
        fs::write(&path, b"application image fixture").unwrap();
        let Some(captured) = capture_path(&path) else {
            panic!("NTFS leaf capture should succeed on the validation volume");
        };
        assert!(ordinary_file(HANDLE(captured._leaf.as_raw_handle().cast())));
        assert!(
            fs::OpenOptions::new().write(true).open(&path).is_err(),
            "session leaf must not share write access"
        );

        let moved_leaf = parent.join("application-moved.exe");
        fs::rename(&path, &moved_leaf).unwrap();
        // 祖先 route handle が残っていないことを親 directory の DELETE access で確かめる。
        let parent_delete = fs::OpenOptions::new()
            .access_mode(windows::Win32::Storage::FileSystem::DELETE.0)
            .share_mode(
                (windows::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE)
                    .0,
            )
            .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(&parent)
            .expect("the captured session must not retain ancestor route handles");
        drop(parent_delete);
        assert_eq!(fs::read(&moved_leaf).unwrap(), b"application image fixture");
        drop(captured);
        directory
            .close()
            .expect("the captured leaf and fixture contents should be released");
    }

    #[test]
    fn operation_route_accepts_same_image_and_rejects_replacement() {
        let (_directory, path) = temp_image();
        let Some(captured) = capture_path(&path) else {
            panic!("NTFS leaf capture should succeed on the validation volume");
        };
        let operation = captured
            .lease()
            .expect("unchanged route matches startup identity");
        let moved = path.with_extension("old");
        assert!(
            fs::rename(&path, &moved).is_err(),
            "operation route pins the image path"
        );
        drop(operation);

        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement B").unwrap();
        assert!(captured.lease().is_none());
        assert_eq!(fs::read(&path).unwrap(), b"replacement B");
        assert_eq!(fs::read(&moved).unwrap(), b"application image fixture");
    }
}
