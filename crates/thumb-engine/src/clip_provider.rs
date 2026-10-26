//! CLIP STUDIO PAINT (.clip) サムネイルプロバイダ（内部実装）
//!
//! ## .clip ファイルの構造
//!
//! .clip ファイルは独自チャンク形式（`CSFCHUNK` ヘッダ）で始まり、
//! ファイルの末尾付近に SQLite 3 データベースが埋め込まれている。
//!
//! SQLite の `CanvasPreview` テーブルに PNG 形式のサムネイルが格納されている:
//! ```sql
//! SELECT ImageData FROM CanvasPreview LIMIT 1
//! ```
//!
//! ## 実装方針
//!
//! 1. ファイル全体を読み込んで `SQLite format 3` マジックのオフセットを探す
//! 2. そのオフセット以降のバイト列を rusqlite でインメモリ DB として開く
//! 3. `CanvasPreview.ImageData` から PNG blob を取得してデコード

use std::{
    io::{Read, Seek},
    path::Path,
};

use image::ImageReader;
use rusqlite::{Connection, DatabaseName, ffi, serialize::OwnedData};

use crate::{FitMode, ThumbnailError, ThumbnailResult};

const SUPPORTED_EXTENSIONS: &[&str] = &["clip"];

/// SQLite ファイルの先頭マジックバイト列
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

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

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };

    // ファイル内の SQLite マジックを探す
    let sqlite_offset = match find_sqlite_offset(&data) {
        Some(off) => off,
        None => {
            return ThumbnailResult::Failed(ThumbnailError::Decode(
                "SQLite database not found in .clip file".to_string(),
            ));
        }
    };

    // SQLite 部分だけを取り出してインメモリ DB として開く
    let sqlite_bytes = &data[sqlite_offset..];
    let png_bytes = match extract_preview_png(sqlite_bytes) {
        Ok(b) => b,
        Err(e) => return ThumbnailResult::Failed(e),
    };

    decode_and_resize(&png_bytes, size, fit)
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
        .is_some_and(|s| s.eq_ignore_ascii_case("clip"))
    {
        return ThumbnailResult::Unsupported;
    }
    let mut data = Vec::new();
    if let Err(e) = reader.read_to_end(&mut data) {
        return ThumbnailResult::Failed(ThumbnailError::Io(e));
    }
    let Some(offset) = find_sqlite_offset(&data) else {
        return ThumbnailResult::Failed(ThumbnailError::Decode(
            "SQLite database not found in .clip file".into(),
        ));
    };
    match extract_preview_png(&data[offset..]) {
        Ok(bytes) => decode_and_resize(&bytes, size, fit),
        Err(e) => ThumbnailResult::Failed(e),
    }
}

/// バイト列中の SQLite マジックの先頭オフセットを返す
fn find_sqlite_offset(data: &[u8]) -> Option<usize> {
    data.windows(SQLITE_MAGIC.len())
        .position(|w| w == SQLITE_MAGIC)
}

/// SQLite バイト列からプレビュー PNG を取り出す
fn extract_preview_png(sqlite_bytes: &[u8]) -> Result<Vec<u8>, ThumbnailError> {
    // sqlite3_deserialize を使ってバイト列をインメモリ DB として開く。
    // 一時ファイル不要・並列処理でも競合しない。
    let mut conn = Connection::open_in_memory()
        .map_err(|e| ThumbnailError::Decode(format!("SQLite open failed: {e}")))?;

    // OwnedData は sqlite3_malloc で確保したメモリを要求するため、unsafe でコピーする
    let owned = unsafe {
        let sz = sqlite_bytes.len();
        let ptr = ffi::sqlite3_malloc(sz as i32) as *mut u8;
        if ptr.is_null() {
            return Err(ThumbnailError::Decode("sqlite3_malloc failed".to_string()));
        }
        std::ptr::copy_nonoverlapping(sqlite_bytes.as_ptr(), ptr, sz);
        OwnedData::from_raw_nonnull(std::ptr::NonNull::new_unchecked(ptr), sz)
    };
    conn.deserialize(DatabaseName::Main, owned, true)
        .map_err(|e| ThumbnailError::Decode(format!("SQLite deserialize failed: {e}")))?;

    let result: Result<Vec<u8>, _> =
        conn.query_row("SELECT ImageData FROM CanvasPreview LIMIT 1", [], |row| {
            row.get(0)
        });

    let data =
        result.map_err(|e| ThumbnailError::Decode(format!("CanvasPreview query failed: {e}")))?;

    if data.is_empty() {
        return Err(ThumbnailError::Decode(
            "CanvasPreview.ImageData is empty (thumbnail not yet generated)".to_string(),
        ));
    }

    Ok(data)
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
