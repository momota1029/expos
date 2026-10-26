//! gpui_drag_ghost — 任意の GPUI Entity をドラッグゴーストウィンドウにする
//!
//! ## 使い方
//!
//! ```rust,ignore
//! use gpui_drag_ghost::{DragGhostOptions, open_drag_ghost};
//!
//! let root = MyDragGhostRenderer { /* ... */ };
//! let options = DragGhostOptions {
//!     width: 260.0,
//!     height: 48.0,
//!     hotspot_x: 24.0,
//!     hotspot_y: 24.0,
//! };
//! let lease = open_drag_ghost(cx, root, options)?;
//!
//! // lease.native_handle() / offset_x() / offset_y() を DoDragDrop 側に渡す
//! // ドラッグ終了後に:
//! lease.close(cx);
//! ```
//!
//! ## 仕組み
//!
//! 1. GPUI の透明 PopUp ウィンドウを開き、渡された `Render` 実装で中身を描画する
//! 2. Win32 の WS_EX_LAYERED + WS_EX_TRANSPARENT + TOPMOST でマウスを透過させる
//! 3. `DoDragDrop` の `GiveFeedback` コールバックで `SetWindowPos` を呼んで追従させる
//!    （追従部分は `win_shell_dnd` crate の責務）
//! 4. ドラッグ終了後に `DragGhostLease::close()` でウィンドウを破棄する

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Render, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, point, px, size,
};

// ---------------------------------------------------------------------------
// 公開型
// ---------------------------------------------------------------------------

/// ゴーストウィンドウの設定
pub struct DragGhostOptions {
    /// ゴーストウィンドウの論理サイズ（幅）
    pub width: f32,
    /// ゴーストウィンドウの論理サイズ（高さ）
    pub height: f32,
    /// カーソルからのホットスポット X（論理ピクセル）
    pub hotspot_x: f32,
    /// カーソルからのホットスポット Y（論理ピクセル）
    pub hotspot_y: f32,
}

/// ゴーストウィンドウのネイティブ情報（Win32 側に渡す用）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragGhostNative {
    pub handle: isize,
    pub offset_x: i32,
    pub offset_y: i32,
}

/// ゴーストウィンドウのリース（開閉管理）
///
/// Drop しても自動では閉じない。明示的に `close()` / `close_async()` を呼ぶこと。
#[derive(Clone, Debug)]
pub struct DragGhostLease {
    window: AnyWindowHandle,
    native: DragGhostNative,
}

impl DragGhostLease {
    /// Win32 に渡すネイティブ情報
    pub fn native(&self) -> DragGhostNative {
        self.native
    }

    /// ネイティブウィンドウハンドル（HWND の isize 表現）
    pub fn native_handle(&self) -> isize {
        self.native.handle
    }

    /// カーソルからの X オフセット（物理ピクセル）
    pub fn offset_x(&self) -> i32 {
        self.native.offset_x
    }

    /// カーソルからの Y オフセット（物理ピクセル）
    pub fn offset_y(&self) -> i32 {
        self.native.offset_y
    }

    /// GPUI 側で準備した初回描画を、現在のカーソル位置に表示する。
    pub fn show_at_cursor(&self) -> Result<(), String> {
        show_drag_ghost_platform(self.native)
    }

    /// App コンテキストでゴーストウィンドウを閉じる
    pub fn close(&self, cx: &mut App) {
        let _ = cx.update_window(self.window, |_, window, _| {
            window.remove_window();
        });
    }

    /// AsyncApp コンテキストでゴーストウィンドウを閉じる
    pub fn close_async(&self, cx: &mut gpui::AsyncApp) {
        let _ = cx.update_window(self.window, |_, window, _| {
            window.remove_window();
        });
    }
}

// ---------------------------------------------------------------------------
// 公開関数
// ---------------------------------------------------------------------------

/// 任意の `Render` 実装をドラッグゴーストウィンドウとして開く。
///
/// Windows でのみ実際にウィンドウを開く。他プラットフォームでは `Err` を返す。
pub fn open_drag_ghost<R: Render + 'static>(
    cx: &mut App,
    root: R,
    options: DragGhostOptions,
) -> Result<DragGhostLease, String> {
    open_drag_ghost_platform(cx, root, options)
}

/// AsyncApp 経由で開く。
pub fn open_drag_ghost_async<R: Render + 'static>(
    cx: &mut gpui::AsyncApp,
    root: R,
    options: DragGhostOptions,
) -> Result<DragGhostLease, String> {
    cx.update(|app| open_drag_ghost(app, root, options))
}

// ---------------------------------------------------------------------------
// Windows 実装
// ---------------------------------------------------------------------------

/// Win32 側のサイズ補正用マージン
///
/// なぜ必要か:
/// - GPUI のレンダリング領域と Win32 のウィンドウ境界にわずかなずれがあり、
///   右端・下端がクリップされることを防ぐため。
const SAFE_MARGIN_PHYSICAL: u32 = 2;
const EXTRA_RIGHT_PHYSICAL: u32 = 6;

#[cfg(target_os = "windows")]
fn show_drag_ghost_platform(native: DragGhostNative) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{HWND, POINT},
        UI::WindowsAndMessaging::{
            GetCursorPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos,
        },
    };

    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) }
        .map_err(|error| format!("GetCursorPos failed: {error}"))?;
    unsafe {
        SetWindowPos(
            HWND(native.handle as *mut std::ffi::c_void),
            HWND_TOPMOST,
            cursor.x + native.offset_x,
            cursor.y + native.offset_y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(|error| format!("SetWindowPos failed: {error}"))
}

#[cfg(not(target_os = "windows"))]
fn show_drag_ghost_platform(_native: DragGhostNative) -> Result<(), String> {
    Err("drag ghost windows are only supported on Windows".to_string())
}

#[cfg(target_os = "windows")]
fn open_drag_ghost_platform<R: Render + 'static>(
    cx: &mut App,
    root: R,
    options: DragGhostOptions,
) -> Result<DragGhostLease, String> {
    let (cursor_x, cursor_y) = query_cursor_screen_pos().unwrap_or((120, 120));

    let bounds = Bounds::new(
        point(px(cursor_x as f32), px(cursor_y as f32)),
        size(px(options.width), px(options.height)),
    );

    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: false,
                show: false,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |_window, cx| cx.new(|_| root),
        )
        .map_err(|err| format!("open_window failed: {err}"))?;

    let native = match handle.update(cx, |_root, window, _| extract_native(window, &options)) {
        Ok(Ok(native)) => native,
        Ok(Err(error)) => {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            return Err(error);
        }
        Err(error) => {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            return Err(format!("failed to read ghost window handle: {error}"));
        }
    };

    Ok(DragGhostLease {
        window: handle.into(),
        native,
    })
}

#[cfg(not(target_os = "windows"))]
fn open_drag_ghost_platform<R: Render + 'static>(
    _cx: &mut App,
    _root: R,
    _options: DragGhostOptions,
) -> Result<DragGhostLease, String> {
    Err("drag ghost windows are only supported on Windows".to_string())
}

#[cfg(target_os = "windows")]
fn extract_native(window: &Window, options: &DragGhostOptions) -> Result<DragGhostNative, String> {
    use raw_window_handle::RawWindowHandle;

    let raw = raw_window_handle::HasWindowHandle::window_handle(window)
        .map_err(|err| format!("window_handle failed: {err}"))?;
    let handle = match raw.as_raw() {
        RawWindowHandle::Win32(raw) => raw.hwnd.get(),
        _ => return Err("unexpected non-win32 window handle".to_string()),
    };

    let scale_factor = window.scale_factor().max(1.0);
    let width_px = ((options.width * scale_factor).ceil() as u32)
        + SAFE_MARGIN_PHYSICAL * 2
        + EXTRA_RIGHT_PHYSICAL;
    let height_px = ((options.height * scale_factor).ceil() as u32) + SAFE_MARGIN_PHYSICAL * 2;

    let offset_x =
        -((options.hotspot_x * scale_factor).round() as i32) - SAFE_MARGIN_PHYSICAL as i32;
    let offset_y =
        -((options.hotspot_y * scale_factor).round() as i32) - SAFE_MARGIN_PHYSICAL as i32;

    configure_window_styles(handle, width_px, height_px)?;

    Ok(DragGhostNative {
        handle,
        offset_x,
        offset_y,
    })
}

/// GPUI ウィンドウを「マウス透過・最前面・ツールウィンドウ」に設定する。
///
/// なぜこれらのスタイルが必要か:
/// - WS_EX_TRANSPARENT: マウスクリックがゴーストを貫通して下のウィンドウに届く
/// - WS_EX_TOOLWINDOW: Alt+Tab に表示されない
/// - WS_EX_NOACTIVATE: フォーカスを奪わない
/// - WS_EX_LAYERED + LWA_ALPHA: 透明度制御（GPUI の透明背景と併用）
/// - HWND_TOPMOST: 常に最前面
#[cfg(target_os = "windows")]
fn configure_window_styles(hwnd: isize, width: u32, height: u32) -> Result<(), String> {
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, HWND_TOPMOST, LWA_ALPHA, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_OVERLAPPEDWINDOW,
        WS_POPUP,
    };

    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let existing_style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    let existing_ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let updated_style = (existing_style & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0;
    let updated_ex = existing_ex
        | WS_EX_LAYERED.0
        | WS_EX_TRANSPARENT.0
        | WS_EX_TOOLWINDOW.0
        | WS_EX_NOACTIVATE.0;

    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, updated_style as isize);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, updated_ex as isize);
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)
            .map_err(|err| format!("SetLayeredWindowAttributes failed: {err}"))?;
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            width as i32,
            height as i32,
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn query_cursor_screen_pos() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_ok() {
        Some((pt.x, pt.y))
    } else {
        None
    }
}
