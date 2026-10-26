//! 通常ファイルを OS の既定アプリへ渡す adapter。
//!
//! v5 の `file-view/src/platform_open.rs` と同じく `rundll32.exe` の
//! `FileProtocolHandler` を使う。ShellExecute を直接呼ぶより、関連付けの解決を
//! OS に任せられる。起動は呼び出し側が描画後に回す（`Window::defer`）。

use std::path::Path;
use std::process::Command;

/// 既定アプリで開く。起動できたかどうかだけを返し、終了は待たない。
pub fn open_in_default_app(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("path is empty".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        // コンソールウィンドウを出さない。v5 と同じ起動方法。
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new("rundll32.exe");
        command
            .arg("url.dll,FileProtocolHandler")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW);
        return command
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("rundll32.exe failed to spawn: {error}"));
    }

    #[cfg(target_os = "macos")]
    {
        return Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("open failed to spawn: {error}"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("xdg-open failed to spawn: {error}"));
    }

    #[allow(unreachable_code)]
    Err("unsupported platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_is_rejected_before_spawning() {
        assert!(open_in_default_app(Path::new("")).is_err());
    }
}
