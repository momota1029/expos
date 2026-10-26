//! MediBang Paint Pro (.mdp) サムネイルプロバイダ（内部実装）
//!
//! ## .mdp ファイルの構造
//!
//! ```text
//! [0x00] magic:      b"mdipack\0" (8 bytes)
//! [0x08] unknown:    u32 LE  (バージョン等)
//! [0x0C] xml_size:   u32 LE
//! [0x10] total_size: u32 LE
//! [0x14] XML:        xml_size bytes
//!          → <Thumb width="W" height="H" bin="thumb" /> が含まれる
//! [0x14+xml_size] PAC チャンク群（複数連続）
//!   PAC チャンク:
//!     magic:   b"PAC " (4 bytes)
//!     size:    u32 LE  (magic の次から PAC チャンク末尾まで)
//!     count:   u32 LE  (エントリ数)
//!     entries: [Entry; count]
//!       Entry (120 bytes):
//!         compressed_size:   u32 LE  (圧縮後バイト数)
//!         decompressed_size: u32 LE  (展開後バイト数)
//!         unknown:           [u8; 48]
//!         name:              [u8; 64]  (null-padded ASCII)
//!     data_pool: エントリ順に zlib 圧縮データが連続
//!
//! ## サムネイル取得手順
//!
//! 1. XML から <Thumb width=W height=H /> を読んでサイズを取得
//! 2. PAC チャンクを順に走査し、name == "thumb" のエントリを探す
//! 3. そのエントリの zlib データを展開
//! 4. 展開後データは W×H×4 bytes の BGRA 生ピクセル
//! 5. BGRA → RGBA 変換して DynamicImage に変換

use std::{
    io::{Read, Seek},
    path::Path,
};

use crate::{FitMode, ThumbnailError, ThumbnailResult};

const MAGIC: &[u8] = b"mdipack\0";
const PAC_MAGIC: &[u8] = b"PAC ";
const ENTRY_SIZE: usize = 120;
const ENTRY_COMPRESSED_SIZE_OFFSET: usize = 0;
const ENTRY_DECOMPRESSED_SIZE_OFFSET: usize = 4;
const ENTRY_NAME_OFFSET: usize = 56;
const ENTRY_NAME_SIZE: usize = 64;

pub(crate) fn generate(path: &Path, size: u32, fit: &FitMode) -> ThumbnailResult {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());

    match ext.as_deref() {
        Some("mdp") => {}
        _ => return ThumbnailResult::Unsupported,
    }

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };

    match extract_thumb(&data, size, fit) {
        Ok(img) => ThumbnailResult::Generated(img),
        Err(e) => ThumbnailResult::Failed(e),
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
        .is_some_and(|s| s.eq_ignore_ascii_case("mdp"))
    {
        return ThumbnailResult::Unsupported;
    }
    let mut data = Vec::new();
    if let Err(e) = reader.read_to_end(&mut data) {
        return ThumbnailResult::Failed(ThumbnailError::Io(e));
    }
    match extract_thumb(&data, size, fit) {
        Ok(img) => ThumbnailResult::Generated(img),
        Err(e) => ThumbnailResult::Failed(e),
    }
}

fn extract_thumb(
    data: &[u8],
    size: u32,
    fit: &FitMode,
) -> Result<image::DynamicImage, ThumbnailError> {
    // magic チェック
    if data.len() < 20 || &data[..8] != MAGIC {
        return Err(ThumbnailError::Decode("not an mdipack file".into()));
    }

    let xml_size = u32::from_le_bytes(data[0x0C..0x10].try_into().unwrap()) as usize;
    let xml_end = 0x14 + xml_size;
    if data.len() < xml_end {
        return Err(ThumbnailError::Decode("truncated mdipack XML".into()));
    }

    // XML から Thumb のサイズを取得
    let xml_str = std::str::from_utf8(&data[0x14..xml_end])
        .map_err(|_| ThumbnailError::Decode("invalid UTF-8 in XML".into()))?;
    let (thumb_w, thumb_h) = parse_thumb_size(xml_str)?;

    // PAC チャンクを走査して name="thumb" のデータを探す
    let bgra = find_thumb_data(data, xml_end, thumb_w, thumb_h)?;

    // BGRA → RGBA 変換
    let mut rgba = vec![0u8; bgra.len()];
    for i in (0..bgra.len()).step_by(4) {
        rgba[i] = bgra[i + 2]; // R
        rgba[i + 1] = bgra[i + 1]; // G
        rgba[i + 2] = bgra[i]; // B
        rgba[i + 3] = bgra[i + 3]; // A
    }

    let img = image::RgbaImage::from_raw(thumb_w, thumb_h, rgba)
        .map(image::DynamicImage::ImageRgba8)
        .ok_or_else(|| ThumbnailError::Decode("BGRA buffer size mismatch".into()))?;

    let result = match fit {
        FitMode::Fit => crate::image_provider::fit(&img, size),
        FitMode::Cover => crate::image_provider::cover(&img, size),
    };

    Ok(result)
}

/// XML 文字列から `<Thumb width="W" height="H" .../>` を解析する
fn parse_thumb_size(xml: &str) -> Result<(u32, u32), ThumbnailError> {
    // 簡易パース: <Thumb ... width="W" ... height="H" .../>
    let thumb_tag = xml
        .find("<Thumb ")
        .map(|i| &xml[i..])
        .and_then(|s| s.find('>').map(|j| &s[..=j]));

    let tag = thumb_tag.ok_or_else(|| ThumbnailError::Decode("<Thumb> tag not found".into()))?;

    let w = parse_attr(tag, "width")
        .ok_or_else(|| ThumbnailError::Decode("Thumb width not found".into()))?;
    let h = parse_attr(tag, "height")
        .ok_or_else(|| ThumbnailError::Decode("Thumb height not found".into()))?;

    Ok((w, h))
}

/// 属性値を文字列から取り出す（`key="value"` 形式）
fn parse_attr(s: &str, key: &str) -> Option<u32> {
    let needle = format!("{key}=\"");
    let start = s.find(&needle)? + needle.len();
    let end = s[start..].find('"')? + start;
    s[start..end].parse().ok()
}

/// PAC チャンクを走査して "thumb" エントリのデータを返す（BGRA 生ピクセル）
fn find_thumb_data(
    data: &[u8],
    pac_start: usize,
    expected_w: u32,
    expected_h: u32,
) -> Result<Vec<u8>, ThumbnailError> {
    let expected_size = (expected_w * expected_h * 4) as usize;
    let mut pos = pac_start;

    while pos + 12 <= data.len() {
        if &data[pos..pos + 4] != PAC_MAGIC {
            break;
        }

        let pac_size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(data[pos + 8..pos + 12].try_into().unwrap()) as usize;
        let chunk_end = pos + 4 + pac_size;
        if chunk_end > data.len() {
            break;
        }

        // エントリ全体とデータプールが収まるかチェック
        let entries_end = pos + 12 + count * ENTRY_SIZE;
        if entries_end > chunk_end {
            break;
        }

        // data_pool の先頭（エントリ群の直後）
        let pool_start = entries_end;

        // プール内のオフセットを追跡しながらエントリを走査
        let mut pool_offset = 0usize;
        for i in 0..count {
            let ep = pos + 12 + i * ENTRY_SIZE;
            let comp_size = u32::from_le_bytes(
                data[ep + ENTRY_COMPRESSED_SIZE_OFFSET..ep + ENTRY_COMPRESSED_SIZE_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let decomp_size = u32::from_le_bytes(
                data[ep + ENTRY_DECOMPRESSED_SIZE_OFFSET..ep + ENTRY_DECOMPRESSED_SIZE_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let name = parse_entry_name(
                &data[ep + ENTRY_NAME_OFFSET..ep + ENTRY_NAME_OFFSET + ENTRY_NAME_SIZE],
            );

            let abs = pool_start + pool_offset;
            if abs + comp_size > chunk_end {
                return Err(ThumbnailError::Decode("PAC data pool out of bounds".into()));
            }

            let decompressed = decompress_zlib(&data[abs..abs + comp_size])?;
            pool_offset += comp_size;

            if name == b"thumb" {
                if decompressed.len() != decomp_size {
                    return Err(ThumbnailError::Decode(format!(
                        "thumb decompressed size mismatch: got {} expected {}",
                        decompressed.len(),
                        decomp_size
                    )));
                }
                if decompressed.len() != expected_size {
                    return Err(ThumbnailError::Decode(format!(
                        "unexpected thumb data size: got {} expected {}",
                        decompressed.len(),
                        expected_size
                    )));
                }
                return Ok(decompressed);
            }
        }

        // 次の PAC チャンクへ
        // pac_size は magic の次から計測される (size フィールド以降のバイト数)
        pos = chunk_end;
    }

    Err(ThumbnailError::Decode(
        "thumb entry not found in .mdp PAC chunks".into(),
    ))
}

/// zlib (deflate) 展開
fn decompress_zlib(compressed: &[u8]) -> Result<Vec<u8>, ThumbnailError> {
    use std::io::{Cursor, Read as _};

    if compressed.len() < 2 {
        return Err(ThumbnailError::Decode("empty compressed data".into()));
    }

    let mut decoder = flate2::read::ZlibDecoder::new(Cursor::new(compressed));
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| ThumbnailError::Decode(format!("zlib decompress failed: {e}")))?;

    Ok(out)
}

/// エントリ名を取り出す（NUL/空白パディングを除去）。
fn parse_entry_name(raw: &[u8]) -> &[u8] {
    let nul = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let mut s = &raw[..nul];
    while let Some(first) = s.first() {
        if *first == b' ' {
            s = &s[1..];
        } else {
            break;
        }
    }
    while let Some(last) = s.last() {
        if *last == b' ' {
            s = &s[..s.len() - 1];
        } else {
            break;
        }
    }
    s
}
