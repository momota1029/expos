//! Krita (.kra) サムネイルプロバイダ（内部実装）
//!
//! ## .kra ファイルの構造
//!
//! .kra は ZIP アーカイブ。内部に `preview.png`（最大 256px）が含まれる。
//!
//! ```text
//! .kra (ZIP)
//! ├── mimetype          # "application/x-krita"
//! ├── preview.png       # サムネイル（最大 256px）← ここを使う
//! ├── mergedimage.png   # フル解像度合成画像（古いファイルには無い場合あり）
//! └── maindoc.xml
//! ```
//!
//! ## 実装方針
//!
//! 1. ZIP として開く
//! 2. `preview.png` を読み出す（なければ `mergedimage.png` にフォールバック）
//! 3. image クレートでデコード・リサイズ

use std::io::{Read, Seek};
use std::path::Path;

use image::ImageReader;
use zip::ZipArchive;

use crate::{FitMode, ThumbnailError, ThumbnailResult};

const SUPPORTED_EXTENSIONS: &[&str] = &["kra", "krz"];

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

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };

    let mut archive = match ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => {
            return ThumbnailResult::Failed(ThumbnailError::Decode(format!(
                "ZIP open failed: {e}"
            )));
        }
    };

    // preview.png を優先し、なければ mergedimage.png を試みる
    let png_bytes = read_zip_entry(&mut archive, "preview.png")
        .or_else(|| read_zip_entry(&mut archive, "mergedimage.png"));

    match png_bytes {
        Some(bytes) => decode_and_resize(&bytes, size, fit),
        None => ThumbnailResult::Failed(ThumbnailError::Decode(
            "preview.png not found in .kra archive".to_string(),
        )),
    }
}

pub(crate) fn generate_reader<R: Read + Seek>(
    path: &Path,
    reader: &mut R,
    size: u32,
    fit: &FitMode,
) -> ThumbnailResult {
    if !path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "kra" | "krz"))
    {
        return ThumbnailResult::Unsupported;
    }
    let mut archive = match ZipArchive::new(reader) {
        Ok(a) => a,
        Err(e) => {
            return ThumbnailResult::Failed(ThumbnailError::Decode(format!(
                "ZIP open failed: {e}"
            )));
        }
    };
    match read_zip_entry(&mut archive, "preview.png")
        .or_else(|| read_zip_entry(&mut archive, "mergedimage.png"))
    {
        Some(bytes) => decode_and_resize(&bytes, size, fit),
        None => ThumbnailResult::Failed(ThumbnailError::Decode(
            "preview.png not found in .kra archive".into(),
        )),
    }
}

/// ZIP アーカイブから指定エントリを読み出す。失敗したら None。
fn read_zip_entry<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let mut entry = archive.by_name(name).ok()?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).ok()?;
    if buf.is_empty() { None } else { Some(buf) }
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
