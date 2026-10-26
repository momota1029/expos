//! `image` クレートによる静止画サムネイルプロバイダ（内部実装）
//!
//! PNG / JPEG / GIF / BMP / WebP / TIFF / ICO に対応。

use std::{
    io::{BufReader, Read, Seek},
    path::Path,
};

use image::{DynamicImage, ImageReader, imageops::FilterType};

use crate::{FitMode, ThumbnailError, ThumbnailResult};

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif", "ico",
];

pub(crate) fn generate(path: &Path, size: u32, fit_mode: &FitMode) -> ThumbnailResult {
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

    let img = match ImageReader::open(path) {
        Ok(reader) => match reader.decode() {
            Ok(img) => img,
            Err(e) => return ThumbnailResult::Failed(ThumbnailError::Decode(e.to_string())),
        },
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };

    let result = match fit_mode {
        FitMode::Fit => fit(&img, size),
        FitMode::Cover => cover(&img, size),
    };

    ThumbnailResult::Generated(result)
}

pub(crate) fn generate_reader<R: Read + Seek>(
    path: &Path,
    reader: &mut R,
    size: u32,
    fit_mode: &FitMode,
) -> ThumbnailResult {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    if !ext
        .as_deref()
        .is_some_and(|ext| SUPPORTED_EXTENSIONS.contains(&ext))
    {
        return ThumbnailResult::Unsupported;
    }
    let img = match ImageReader::new(BufReader::new(reader)).with_guessed_format() {
        Ok(reader) => match reader.decode() {
            Ok(img) => img,
            Err(e) => return ThumbnailResult::Failed(ThumbnailError::Decode(e.to_string())),
        },
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };
    let result = match fit_mode {
        FitMode::Fit => fit(&img, size),
        FitMode::Cover => cover(&img, size),
    };
    ThumbnailResult::Generated(result)
}

/// 長辺を size に収めるようにリサイズする（余白あり）
pub(crate) fn fit(img: &DynamicImage, size: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let scale = (size as f32 / w as f32).min(size as f32 / h as f32);
    let new_w = (w as f32 * scale).ceil().max(1.0) as u32;
    let new_h = (h as f32 * scale).ceil().max(1.0) as u32;
    let filter = pick_filter(w, h, new_w, new_h);
    img.resize_exact(new_w, new_h, filter)
}

/// 短辺を size に合わせてリサイズし、中央を size×size でクロップする
pub(crate) fn cover(img: &DynamicImage, size: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());

    // 短辺が size になるようなスケール
    let scale = if w < h {
        size as f32 / w as f32
    } else {
        size as f32 / h as f32
    };

    let new_w = (w as f32 * scale).ceil() as u32;
    let new_h = (h as f32 * scale).ceil() as u32;

    let filter = pick_filter(w, h, new_w, new_h);
    let resized = img.resize_exact(new_w, new_h, filter);

    // 中央から size×size を切り出す
    let x = (resized.width().saturating_sub(size)) / 2;
    let y = (resized.height().saturating_sub(size)) / 2;
    resized.crop_imm(x, y, size, size)
}

/// 学習ポイント:
/// - 縮小時は Lanczos3 が高品質
/// - 拡大時は CatmullRom の方がリンギングが出にくい
fn pick_filter(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> FilterType {
    if dst_w > src_w || dst_h > src_h {
        FilterType::CatmullRom
    } else {
        FilterType::Lanczos3
    }
}
