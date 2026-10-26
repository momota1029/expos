//! PSD（Adobe Photoshop）サムネイルプロバイダ（内部実装）
//!
//! `psd` クレートでマージ済み画像データを取得してリサイズする。
//! PSD 内に埋め込みサムネイルがある場合はそちらを優先する。

use std::{
    io::{Read, Seek},
    path::Path,
};

use image::{DynamicImage, RgbaImage};

use crate::{FitMode, ThumbnailError, ThumbnailResult};

const SUPPORTED_EXTENSIONS: &[&str] = &["psd", "psb"];
const MAX_DECODED_BITMAP_BYTES: usize = 64 * 1024 * 1024;

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

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Io(e)),
    };

    generate_bytes(&bytes, size, fit)
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
        .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "psd" | "psb"))
    {
        return ThumbnailResult::Unsupported;
    }
    let mut bytes = Vec::new();
    if let Err(e) = reader.read_to_end(&mut bytes) {
        return ThumbnailResult::Failed(ThumbnailError::Io(e));
    }
    generate_bytes(&bytes, size, fit)
}

fn generate_bytes(bytes: &[u8], size: u32, fit: &FitMode) -> ThumbnailResult {
    if bytes.get(..6).is_some_and(|header| {
        header.get(..4) == Some(b"8BPS") && header.get(4..6) == Some(&2u16.to_be_bytes())
    }) {
        return generate_psb(bytes, size, fit);
    }

    let psd = match psd::Psd::from_bytes(bytes) {
        Ok(p) => p,
        Err(e) => return ThumbnailResult::Failed(ThumbnailError::Decode(format!("{e:?}"))),
    };
    let rgba_bytes = psd.rgba();
    let img = match RgbaImage::from_raw(psd.width(), psd.height(), rgba_bytes.to_vec()) {
        Some(img) => DynamicImage::ImageRgba8(img),
        None => {
            return ThumbnailResult::Failed(ThumbnailError::Decode(
                "failed to build RGBA image from PSD data".into(),
            ));
        }
    };
    ThumbnailResult::Generated(match fit {
        FitMode::Fit => crate::image_provider::fit(&img, size),
        FitMode::Cover => crate::image_provider::cover(&img, size),
    })
}

fn generate_psb(bytes: &[u8], size: u32, fit: &FitMode) -> ThumbnailResult {
    let options = ag_psd::psd::ReadOptions {
        skip_layer_image_data: Some(true),
        skip_thumbnail: Some(true),
        total_memory_limit: Some(MAX_DECODED_BITMAP_BYTES),
        use_image_data: Some(true),
        ..Default::default()
    };
    let psd = match ag_psd::read_psd(bytes, &options) {
        Ok(psd) => psd,
        Err(error) => return ThumbnailResult::Failed(ThumbnailError::Decode(format!("{error:?}"))),
    };
    let Some(pixels) = psd.image_data else {
        return ThumbnailResult::Failed(ThumbnailError::Decode(
            "PSB has no decoded composite image".into(),
        ));
    };
    let Some(expected_len) = (pixels.width as usize)
        .checked_mul(pixels.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return ThumbnailResult::Failed(ThumbnailError::Decode("PSB dimensions overflow".into()));
    };
    if expected_len > MAX_DECODED_BITMAP_BYTES || pixels.data.len() != expected_len {
        return ThumbnailResult::Failed(ThumbnailError::Decode(
            "PSB composite exceeds bitmap budget or has invalid pixels".into(),
        ));
    }
    let Some(image) = RgbaImage::from_raw(pixels.width, pixels.height, pixels.data) else {
        return ThumbnailResult::Failed(ThumbnailError::Decode(
            "failed to build RGBA image from PSB data".into(),
        ));
    };
    let image = DynamicImage::ImageRgba8(image);
    ThumbnailResult::Generated(match fit {
        FitMode::Fit => crate::image_provider::fit(&image, size),
        FitMode::Cover => crate::image_provider::cover(&image, size),
    })
}
