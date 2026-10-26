//! 内部フォールバックチェーン
//!
//! feature が有効なプロバイダを登録順に試す。
//! `Unsupported` なら次へ、`Generated` か `Failed` で即返す。
//!
//! 順序:
//! 1. image  — 静止画（PNG/JPEG/WebP/GIF/BMP/TIFF/ICO）
//! 2. psd    — Adobe Photoshop（.psd）
//! 3. clip   — CLIP STUDIO PAINT（.clip）
//! 4. kra    — Krita（.kra / .krz）
//! 5. mdp    — MediBang Paint Pro（.mdp）
//! 6. ffmpeg — 動画（mp4/mkv/avi 等）、ffmpeg が PATH にある場合のみ

use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use crate::{FitMode, ThumbnailResult};

pub(crate) fn run(path: &Path, size: u32, fit: FitMode) -> ThumbnailResult {
    #[cfg(feature = "image")]
    {
        let result = crate::image_provider::generate(path, size, &fit);
        if !matches!(result, ThumbnailResult::Unsupported) {
            return result;
        }
    }

    #[cfg(feature = "psd")]
    {
        let result = crate::psd_provider::generate(path, size, &fit);
        if !matches!(result, ThumbnailResult::Unsupported) {
            return result;
        }
    }

    #[cfg(feature = "clip")]
    {
        let result = crate::clip_provider::generate(path, size, &fit);
        if !matches!(result, ThumbnailResult::Unsupported) {
            return result;
        }
    }

    #[cfg(feature = "kra")]
    {
        let result = crate::kra_provider::generate(path, size, &fit);
        if !matches!(result, ThumbnailResult::Unsupported) {
            return result;
        }
    }

    #[cfg(feature = "mdp")]
    {
        let result = crate::mdp_provider::generate(path, size, &fit);
        if !matches!(result, ThumbnailResult::Unsupported) {
            return result;
        }
    }

    #[cfg(feature = "ffmpeg")]
    {
        let result = crate::ffmpeg_provider::generate(path, size, &fit);
        if !matches!(result, ThumbnailResult::Unsupported) {
            return result;
        }
    }

    ThumbnailResult::Unsupported
}

pub(crate) fn run_reader<R: Read + Seek>(
    path: &Path,
    reader: &mut R,
    size: u32,
    fit: FitMode,
) -> ThumbnailResult {
    macro_rules! try_provider {
        ($provider:expr) => {{
            if reader.seek(SeekFrom::Start(0)).is_err() {
                return ThumbnailResult::Failed(crate::ThumbnailError::Decode(
                    "reader seek failed".into(),
                ));
            }
            let result = $provider;
            if !matches!(result, ThumbnailResult::Unsupported) {
                return result;
            }
        }};
    }
    #[cfg(feature = "image")]
    {
        try_provider!(crate::image_provider::generate_reader(
            path, reader, size, &fit
        ));
    }
    #[cfg(feature = "psd")]
    {
        try_provider!(crate::psd_provider::generate_reader(
            path, reader, size, &fit
        ));
    }
    #[cfg(feature = "clip")]
    {
        try_provider!(crate::clip_provider::generate_reader(
            path, reader, size, &fit
        ));
    }
    #[cfg(feature = "kra")]
    {
        try_provider!(crate::kra_provider::generate_reader(
            path, reader, size, &fit
        ));
    }
    #[cfg(feature = "mdp")]
    {
        try_provider!(crate::mdp_provider::generate_reader(
            path, reader, size, &fit
        ));
    }
    ThumbnailResult::Unsupported
}
