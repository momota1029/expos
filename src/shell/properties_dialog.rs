use std::path::Path;

/// Start the native Properties dialog without blocking the GPUI thread.
pub(super) fn open(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("properties dialog received an empty path".to_string());
    }

    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        return std::thread::Builder::new()
            .name("v6-properties-dialog".to_string())
            .spawn(move || {
                if let Err(error) = show_windows_properties_dialog(&path) {
                    eprintln!("[v6][properties-dialog] {}: {error}", path.display());
                }
            })
            .map(|_| ())
            .map_err(|error| format!("could not start properties dialog thread: {error}"));
    }

    #[cfg(not(windows))]
    {
        let _ = path;
        Err("native Properties dialog is unsupported on this platform".to_string())
    }
}

#[cfg(windows)]
fn show_windows_properties_dialog(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::{
        Win32::{
            Foundation::{GetLastError, HWND, RPC_E_CHANGED_MODE},
            System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            UI::Shell::{SHOP_FILEPATH, SHObjectProperties},
        },
        core::PCWSTR,
    };

    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // 専用threadをSTAとして初期化し、モーダルな Shell property sheet をそのthreadに閉じる。
    let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if !result.is_ok() && result != RPC_E_CHANGED_MODE {
        return Err(format!("CoInitializeEx failed: 0x{:08X}", result.0 as u32));
    }
    let should_uninitialize = result.is_ok();

    let opened = unsafe {
        SHObjectProperties(
            HWND::default(),
            SHOP_FILEPATH,
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
        )
    };
    let outcome = if opened.as_bool() {
        pump_messages_after_property_sheet();
        Ok(())
    } else {
        let error = unsafe { GetLastError() };
        Err(format!(
            "SHObjectProperties returned false; last_error={}",
            error.0
        ))
    };

    if should_uninitialize {
        unsafe { CoUninitialize() };
    }
    outcome
}

#[cfg(windows)]
fn pump_messages_after_property_sheet() {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        },
    };

    // SHObjectProperties は modal sheet を同期表示する。戻った直後も child window の
    // teardown / queued messages を専用thread上で処理する。
    for _ in 0..300 {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
