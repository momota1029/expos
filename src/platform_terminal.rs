//! Windows Terminal の detached launch adapter.

use crate::filesystem_identity::FilesystemIdentity;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub path: std::path::PathBuf,
    pub identity: FilesystemIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchError {
    DirectoryChanged,
    Unavailable,
}

#[cfg(windows)]
struct PinnedRoute {
    _handles: Vec<std::fs::File>,
}

#[cfg(windows)]
fn pin_route(request: &Request) -> Result<PinnedRoute, LaunchError> {
    use ::windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            GetFileInformationByHandle,
        },
    };
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    if !request.path.is_absolute() {
        return Err(LaunchError::DirectoryChanged);
    }
    // Hold every no-follow ancestor without write/delete sharing until CreateProcess admits wt.
    let mut handles = Vec::new();
    let mut route: Vec<_> = request.path.ancestors().collect();
    route.reverse();
    for ancestor in route {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
        let handle = options
            .open(ancestor)
            .map_err(|_| LaunchError::DirectoryChanged)?;
        let metadata = handle
            .metadata()
            .map_err(|_| LaunchError::DirectoryChanged)?;
        if !metadata.is_dir()
            || metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(LaunchError::DirectoryChanged);
        }
        handles.push(handle);
    }
    let directory = handles.last().ok_or(LaunchError::DirectoryChanged)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(directory.as_raw_handle() as _), &mut info) }
        .map_err(|_| LaunchError::DirectoryChanged)?;
    let identity = FilesystemIdentity::from_windows_parts(
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    );
    if identity != request.identity {
        return Err(LaunchError::DirectoryChanged);
    }
    Ok(PinnedRoute { _handles: handles })
}

pub(crate) fn launch(request: &Request) -> Result<(), LaunchError> {
    #[cfg(windows)]
    {
        let _route = pin_route(request)?;
        std::process::Command::new("wt.exe")
            .arg("-d")
            .arg(&request.path)
            .spawn()
            .map(|_| ())
            .map_err(|_| LaunchError::Unavailable)
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err(LaunchError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_keeps_spaces_and_unicode_in_the_path_value() {
        let request = Request {
            path: r"C:\Users\ユーザー\My Folder".into(),
            identity: FilesystemIdentity::from_fallback_token("test".into()),
        };
        assert_eq!(
            request.path.to_string_lossy(),
            r"C:\Users\ユーザー\My Folder"
        );
    }

    #[cfg(windows)]
    #[test]
    fn replaced_directory_identity_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Target Folder");
        std::fs::create_dir(&path).unwrap();
        let wrong = FilesystemIdentity::from_windows_parts(0, 0);
        let request = Request {
            path,
            identity: wrong,
        };
        assert_eq!(launch(&request), Err(LaunchError::DirectoryChanged));
    }

    #[cfg(windows)]
    #[test]
    fn pinned_route_blocks_write_open_until_released() {
        use ::windows::Win32::{
            Foundation::{ERROR_SHARING_VIOLATION, GENERIC_WRITE},
            Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
                FILE_SHARE_WRITE,
            },
        };
        use std::os::windows::fs::OpenOptionsExt;

        fn write_open(path: &std::path::Path) -> std::io::Result<std::fs::File> {
            let mut options = std::fs::OpenOptions::new();
            options
                .access_mode(GENERIC_WRITE.0)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
            options.open(path)
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        let identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &path,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        write_open(&path).expect("write-capable directory open before pin");
        let route = pin_route(&Request {
            path: path.clone(),
            identity,
        })
        .expect("pin accepted directory route");
        let error = write_open(&path).expect_err("held read lease excludes write access");
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION.0 as i32),
            "lease denial is a sharing violation, not an access check"
        );
        drop(route);
        write_open(&path).expect("write-capable directory open after release");
    }

    #[cfg(windows)]
    #[test]
    fn pinned_route_and_file_write_attributes_junction_probe() {
        use ::windows::Win32::{
            Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, HANDLE},
            Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                FILE_WRITE_ATTRIBUTES, GetFileInformationByHandle,
            },
            System::IO::DeviceIoControl,
        };
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};

        const DRIVE_FIXED: u32 = 3;
        const FSCTL_SET_REPARSE_POINT: u32 = 589_988;
        const FSCTL_DELETE_REPARSE_POINT: u32 = 589_996;
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

        fn identity(handle: &std::fs::File) -> FilesystemIdentity {
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe { GetFileInformationByHandle(HANDLE(handle.as_raw_handle() as _), &mut info) }
                .expect("query fixture handle identity");
            FilesystemIdentity::from_windows_parts(
                info.dwVolumeSerialNumber,
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            )
        }

        fn open_with_access(
            path: &std::path::Path,
            access: u32,
            no_follow: bool,
        ) -> std::io::Result<std::fs::File> {
            let mut options = std::fs::OpenOptions::new();
            options
                .access_mode(access)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
                .custom_flags(
                    FILE_FLAG_BACKUP_SEMANTICS.0
                        | if no_follow {
                            FILE_FLAG_OPEN_REPARSE_POINT.0
                        } else {
                            0
                        },
                );
            options.open(path)
        }

        let root = std::env::current_dir()
            .expect("workspace current directory")
            .join("target");
        if !root.to_string_lossy().starts_with(r"C:\") {
            eprintln!(
                "SKIP: workspace target fixture root is not on C: ({})",
                root.display()
            );
            return;
        }
        if !root.is_dir() {
            eprintln!(
                "SKIP: workspace target fixture root is missing ({})",
                root.display()
            );
            return;
        }
        let volume: Vec<u16> = r"C:\".encode_utf16().chain(Some(0)).collect();
        let mut fs_name = [0u16; 260];
        let queried = unsafe {
            ::windows::Win32::Storage::FileSystem::GetVolumeInformationW(
                ::windows::core::PCWSTR(volume.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs_name),
            )
        };
        if let Err(error) = queried {
            eprintln!("SKIP: cannot query C: volume information: {error}");
            return;
        }
        let fs_len = fs_name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(fs_name.len());
        let fs_name = String::from_utf16_lossy(&fs_name[..fs_len]);
        let drive_type = unsafe {
            ::windows::Win32::Storage::FileSystem::GetDriveTypeW(::windows::core::PCWSTR(
                volume.as_ptr(),
            ))
        };
        if fs_name != "NTFS" || drive_type != DRIVE_FIXED {
            eprintln!(
                "SKIP: fixture requires local NTFS on C:, found {fs_name}, drive type {drive_type}"
            );
            return;
        }

        let temp = match tempfile::Builder::new()
            .prefix("route-lease-")
            .tempdir_in(&root)
        {
            Ok(temp) => temp,
            Err(error) => {
                eprintln!("SKIP: cannot create disposable C: fixture: {error}");
                return;
            }
        };
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::create_dir(&source).expect("create source directory");
        std::fs::create_dir(&target).expect("create target directory");
        let source_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &source,
            crate::directory::EntryKind::Folder,
        )
        .expect("read source identity")
        .filesystem_identity();
        let target_identity = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &target,
            crate::directory::EntryKind::Folder,
        )
        .expect("read target identity")
        .filesystem_identity();
        let route = pin_route(&Request {
            path: source.clone(),
            identity: source_identity.clone(),
        })
        .expect("pin source route");
        let mutation = match open_with_access(&source, FILE_WRITE_ATTRIBUTES.0, true) {
            Ok(handle) => handle,
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32) => {
                eprintln!(
                    "PROBE: FILE_WRITE_ATTRIBUTES open blocked by pinned route (sharing violation)"
                );
                drop(route);
                return;
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32)
                    || error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                eprintln!(
                    "SKIP: FILE_WRITE_ATTRIBUTES fixture open denied by environment permissions: {error}"
                );
                drop(route);
                return;
            }
            Err(error) => {
                panic!("unexpected FILE_WRITE_ATTRIBUTES fixture open error: {error}");
            }
        };
        assert_eq!(identity(route._handles.last().unwrap()), source_identity);

        let substitute: Vec<u16> = format!(r"\??\{}", target.display())
            .encode_utf16()
            .collect();
        let print_name: Vec<u16> = target.as_os_str().encode_wide().collect();
        let mut path_buffer = substitute.clone();
        path_buffer.push(0);
        path_buffer.extend_from_slice(&print_name);
        path_buffer.push(0);
        let data_length = 8 + path_buffer.len() * 2;
        let mut reparse = vec![0u8; 8 + data_length];
        reparse[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        reparse[4..6].copy_from_slice(&(data_length as u16).to_le_bytes());
        reparse[8..10].copy_from_slice(&0u16.to_le_bytes());
        reparse[10..12].copy_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
        reparse[12..14].copy_from_slice(&(((substitute.len() + 1) * 2) as u16).to_le_bytes());
        reparse[14..16].copy_from_slice(&((print_name.len() * 2) as u16).to_le_bytes());
        for (index, unit) in path_buffer.iter().enumerate() {
            let offset = 16 + index * 2;
            reparse[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let mut set_bytes_returned = 0u32;
        unsafe {
            DeviceIoControl(
                HANDLE(mutation.as_raw_handle() as _),
                FSCTL_SET_REPARSE_POINT,
                Some(reparse.as_ptr().cast()),
                reparse.len() as u32,
                None,
                0,
                Some(&mut set_bytes_returned),
                None,
            )
        }
        .expect("FSCTL_SET_REPARSE_POINT with FILE_WRITE_ATTRIBUTES");

        assert_eq!(identity(route._handles.last().unwrap()), source_identity);
        let followed = open_with_access(&source, FILE_WRITE_ATTRIBUTES.0, false)
            .expect("open source path following installed junction");
        assert_eq!(identity(&followed), target_identity);
        eprintln!(
            "PROBE: FILE_WRITE_ATTRIBUTES junction mutation succeeded; pinned identity stayed {source_identity:?}, followed path resolved to target identity {target_identity:?}"
        );

        let mut delete = [0u8; 8];
        delete[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        let mut delete_bytes_returned = 0u32;
        let removed = unsafe {
            DeviceIoControl(
                HANDLE(mutation.as_raw_handle() as _),
                FSCTL_DELETE_REPARSE_POINT,
                Some(delete.as_ptr().cast()),
                delete.len() as u32,
                None,
                0,
                Some(&mut delete_bytes_returned),
                None,
            )
        };
        assert!(
            removed.is_ok(),
            "remove fixture junction before cleanup: {removed:?}"
        );
        drop(followed);
        drop(mutation);
        drop(route);
        drop(temp);
    }
}
