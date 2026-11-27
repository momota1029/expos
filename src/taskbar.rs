use std::{ffi::OsString, mem::ManuallyDrop};

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_062::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, PROPERTYKEY, WPARAM},
        Storage::EnhancedStorage::{
            PKEY_AppUserModel_ID, PKEY_AppUserModel_RelaunchCommand,
            PKEY_AppUserModel_RelaunchDisplayNameResource, PKEY_AppUserModel_RelaunchIconResource,
        },
        System::{
            Com::StructuredStorage::{
                PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
            },
            Variant::VT_LPWSTR,
        },
        UI::{
            Shell::{
                DefSubclassProc,
                PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
                RemoveWindowSubclass, SHStrDupW, SetCurrentProcessExplicitAppUserModelID,
                SetWindowSubclass,
            },
            WindowsAndMessaging::WM_NCDESTROY,
        },
    },
    core::{HSTRING, PCWSTR, Result},
};

const APP_USER_MODEL_ID: &str = include_str!("../release/AppUserModelID.txt").trim_ascii();

pub(crate) fn set_process_identity() {
    let id = HSTRING::from(APP_USER_MODEL_ID);
    if let Err(error) = unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(id.as_ptr())) } {
        eprintln!("[expos][taskbar] could not set process identity: {error}");
    }
}

pub(crate) fn configure_window(window: &Window) {
    let Some(root) = crate::install_info::install_root() else {
        return;
    };
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut _);
    let launcher = root.join("expos.exe");
    let mut command = OsString::from("\"");
    command.push(&launcher);
    command.push("\"");
    // ランチャーに埋め込んだ先頭のアイコンを使う。
    let mut icon_resource = launcher.into_os_string();
    icon_resource.push(",0");

    if let Err(error) =
        set_relaunch_properties(hwnd, HSTRING::from(&command), HSTRING::from(&icon_resource))
    {
        eprintln!("[expos][taskbar] could not set window relaunch properties: {error}");
    }
}

fn set_relaunch_properties(hwnd: HWND, command: HSTRING, icon_resource: HSTRING) -> Result<()> {
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd)? };
    // HWND の破棄前にプロパティを解放できるよう、作成した UI スレッドで登録する。
    unsafe { SetWindowSubclass(hwnd, Some(on_window_message), 0, 0).ok()? };

    // ID を設定するとタスクバーが情報を読み直すので、起動情報を先に揃える。
    let properties = [
        (PKEY_AppUserModel_RelaunchCommand, command),
        (
            PKEY_AppUserModel_RelaunchDisplayNameResource,
            HSTRING::from(env!("CARGO_PKG_NAME")),
        ),
        (PKEY_AppUserModel_RelaunchIconResource, icon_resource),
        (PKEY_AppUserModel_ID, HSTRING::from(APP_USER_MODEL_ID)),
    ];
    for (key, value) in properties {
        if let Err(error) = set_string(&store, &key, &value) {
            clear_properties(&store);
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(on_window_message), 0) };
            return Err(error);
        }
    }
    Ok(())
}

fn set_string(store: &IPropertyStore, key: &PROPERTYKEY, value: &HSTRING) -> Result<()> {
    let string = unsafe { SHStrDupW(PCWSTR(value.as_ptr()))? };
    let mut variant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                Anonymous: PROPVARIANT_0_0_0 { pwszVal: string },
                ..Default::default()
            }),
        },
    };
    let result = unsafe { store.SetValue(key, &variant) };
    let _ = unsafe { PropVariantClear(&mut variant) };
    result
}

fn clear_properties(store: &IPropertyStore) {
    let empty = PROPVARIANT::default();
    for key in [
        PKEY_AppUserModel_ID,
        PKEY_AppUserModel_RelaunchCommand,
        PKEY_AppUserModel_RelaunchDisplayNameResource,
        PKEY_AppUserModel_RelaunchIconResource,
    ] {
        let _ = unsafe { store.SetValue(&key, &empty) };
    }
}

unsafe extern "system" fn on_window_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        if let Ok(store) = unsafe { SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd) } {
            clear_properties(&store);
        }
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(on_window_message), id) };
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
