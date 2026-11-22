#![cfg_attr(windows, windows_subsystem = "windows")]

use install_layout::{INSTALL_ROOT_ENV, VERSION_DIR_ENV};
use std::env;
use std::io;
use std::path::PathBuf;
use std::process::{self, Command};

fn main() {
    match launch() {
        Ok(code) => process::exit(code),
        Err(error) => {
            let message = format!("Could not start expos.\n\n{error}");
            eprintln!("{message}");
            #[cfg(windows)]
            show_error(&message);
            process::exit(1);
        }
    }
}

fn launch() -> io::Result<i32> {
    let root = launcher_root()?;
    let lock = install_layout::lock_install_root(&root)?;
    let version = install_layout::select_version(&root)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "No installed expos version was found in {}. Run the installer first.",
                root.display()
            ),
        )
    })?;
    let wait = env::var_os("EXPOS_LAUNCHER_WAIT").as_deref() == Some("1".as_ref());
    let mut child = lock
        .spawn_version(&version, |command| {
            command
                .args(env::args_os().skip(1))
                .env(INSTALL_ROOT_ENV, &root)
                .env(VERSION_DIR_ENV, version.path(&root));
            if !wait {
                detach(command);
            }
        })
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{}: {error}", version.executable(&root).display()),
            )
        })?;
    drop(lock);
    if wait {
        return Ok(child.wait()?.code().unwrap_or(1));
    }
    Ok(0)
}

fn launcher_root() -> io::Result<PathBuf> {
    // カスタムルートのショートカットからも、隣にあるインストールを起動する。
    #[cfg(windows)]
    if env::var_os(INSTALL_ROOT_ENV).is_none_or(|root| root.is_empty())
        && let Ok(exe) = env::current_exe()
        && exe
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("expos.exe"))
        && let Some(root) = exe.parent()
        && root.join(".expos-install").is_file()
    {
        return Ok(root.to_path_buf());
    }
    install_layout::install_root()
}

fn detach(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::DETACHED_PROCESS;
        command.creation_flags(DETACHED_PROCESS.0);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{PCWSTR, w};

    let message: Vec<u16> = message.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("expos"),
            MB_OK | MB_ICONERROR,
        );
    }
}
