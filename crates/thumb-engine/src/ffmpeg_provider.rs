//! ffmpeg サブプロセスによる動画サムネイルプロバイダ（内部実装）
//!
//! ffmpeg が PATH に存在する場合のみ動作する。
//! 存在しない場合は `Unsupported` を返す（エラーにしない）。
//!
//! 対応拡張子: mp4 / mkv / avi / mov / wmv / flv / webm / m4v / ts / m2ts
//!
//! # 実装メモ
//! - Windows では `CREATE_NO_WINDOW` フラグでコンソールウィンドウを抑制する
//! - stdout への pipe 出力を使う（CREATE_NO_WINDOW 付きなら動作する）
//! - シーク位置は ffprobe で取得した総再生時間の10%地点（冒頭ロゴ回避）

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use image::ImageReader;

use crate::{FitMode, ThumbnailError, ThumbnailResult};

/// ffmpeg が利用可能かどうかのキャッシュ（プロセス起動ごとに1回だけ確認）
static FFMPEG_AVAILABLE: OnceLock<bool> = OnceLock::new();

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "m2ts",
];

pub(crate) fn generate(path: &Path, size: u32, fit: &FitMode) -> ThumbnailResult {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());

    let Some(ext) = ext else {
        return ThumbnailResult::Unsupported;
    };

    if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
        return ThumbnailResult::Unsupported;
    }

    if !*FFMPEG_AVAILABLE.get_or_init(ffmpeg_available) {
        return ThumbnailResult::Unsupported;
    }

    // 総再生時間の10%地点を取得（失敗したら 0 秒 = 冒頭）
    let seek_sec = probe_duration(path).map(|d| d * 0.1).unwrap_or(0.0);

    // まず指定シークで試み、空なら冒頭で再試行
    let png_bytes = run_ffmpeg(path, seek_sec).or_else(|| {
        if seek_sec > 0.0 {
            run_ffmpeg(path, 0.0)
        } else {
            None
        }
    });

    match png_bytes {
        Some(bytes) => decode_and_resize(&bytes, size, fit),
        None => ThumbnailResult::Failed(ThumbnailError::Process(
            "ffmpeg returned no output".to_string(),
        )),
    }
}

/// ffmpeg でフレームを PNG として stdout から取得する。失敗したら None。
fn run_ffmpeg(input: &Path, seek_sec: f64) -> Option<Vec<u8>> {
    let seek_str = format!("{:.3}", seek_sec);
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-ss",
        &seek_str,
        "-i",
        &input.to_string_lossy(),
        "-vframes",
        "1",
        "-f",
        "image2",
        "-vcodec",
        "png",
        "pipe:1",
    ]);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    no_window(&mut cmd);

    let output = cmd.output().ok()?;
    if output.stdout.is_empty() {
        None
    } else {
        Some(output.stdout)
    }
}

/// ffprobe で総再生時間（秒）を取得する
fn probe_duration(path: &Path) -> Option<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "quiet",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        &path.to_string_lossy(),
    ]);
    cmd.stderr(std::process::Stdio::null());
    no_window(&mut cmd);

    let output = cmd.output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.trim().parse::<f64>().ok()
}

/// PNG バイト列をデコードしてリサイズ
fn decode_and_resize(png_bytes: &[u8], size: u32, fit: &FitMode) -> ThumbnailResult {
    let cursor = std::io::Cursor::new(png_bytes);
    let img = match ImageReader::new(cursor).with_guessed_format() {
        Ok(reader) => match reader.decode() {
            Ok(img) => img,
            Err(e) => return ThumbnailResult::Failed(ThumbnailError::Decode(e.to_string())),
        },
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };

    let result = match fit {
        FitMode::Fit => crate::image_provider::fit(&img, size),
        FitMode::Cover => crate::image_provider::cover(&img, size),
    };

    ThumbnailResult::Generated(result)
}

/// ffmpeg / ffprobe が PATH に存在するか確認する
fn ffmpeg_available() -> bool {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    no_window(&mut cmd);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Windows でコンソールウィンドウを表示しない
///
/// `CREATE_NO_WINDOW`（0x08000000）を creation flags に設定する。
/// std::os::windows::process::CommandExt が標準で提供しており、
/// 外部クレート不要。非Windows では何もしない。
#[cfg(target_os = "windows")]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn no_window(_cmd: &mut Command) {}
