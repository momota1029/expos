use ::windows::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    ERROR_USER_MAPPED_FILE, HANDLE,
};
use ::windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
    LockFileEx, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use ::windows::Win32::System::{IO::OVERLAPPED, Threading::GetCurrentProcess};
use ::windows::core::PCWSTR;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command};

pub(super) fn lock(path: &Path, mut options: OpenOptions) -> io::Result<File> {
    let file = options
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    if super::is_link(&file.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            ".lock is a reparse point",
        ));
    }
    // 同期ハンドルで byte 0 をロックする。PowerShell の FileStream.Lock と同じ範囲。
    let mut offset = OVERLAPPED::default();
    unsafe {
        LockFileEx(
            HANDLE(file.as_raw_handle()),
            LOCKFILE_EXCLUSIVE_LOCK,
            None,
            1,
            0,
            &mut offset,
        )
    }
    .map_err(io::Error::other)?;
    Ok(file)
}

pub(super) fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain([0]).collect()
    }
    let source = wide(source.as_os_str());
    let destination = wide(destination.as_os_str());
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

pub(super) fn is_in_use(exe: &Path) -> io::Result<bool> {
    match File::options()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(exe)
    {
        Ok(_) => Ok(false),
        Err(err)
            if err.raw_os_error().is_some_and(|code| {
                [
                    ERROR_SHARING_VIOLATION.0,
                    ERROR_LOCK_VIOLATION.0,
                    ERROR_USER_MAPPED_FILE.0,
                ]
                .contains(&(code as u32))
            }) =>
        {
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

pub(super) fn spawn_pinned(mut command: Command, exe: &Path) -> io::Result<Child> {
    let pin = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(exe)?;
    let mut child = command.spawn()?;
    let mut child_pin = HANDLE::default();
    // 起動直後に子へハンドルを渡し、ランチャー終了後も実行中の版の削除を防ぐ。
    // child_pin は子プロセス側の値なので、親から CloseHandle しない。
    let result = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(pin.as_raw_handle()),
            HANDLE(child.as_raw_handle()),
            &mut child_pin,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if let Err(err) = result {
        if child.try_wait()?.is_none() {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::other(err));
        }
    }
    Ok(child)
}
