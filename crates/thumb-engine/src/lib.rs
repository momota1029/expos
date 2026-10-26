//! サムネイル生成エンジン
//!
//! # 使い方
//!
//! ```no_run
//! use std::path::Path;
//! use thumb_engine::{generate_thumbnail, FitMode, ThumbnailResult};
//!
//! match generate_thumbnail(Path::new("photo.jpg"), 256, FitMode::Fit) {
//!     ThumbnailResult::Generated(img) => { /* img は image::DynamicImage */ }
//!     ThumbnailResult::Unsupported => { /* アイコンで代替 */ }
//!     ThumbnailResult::Failed(e) => { /* ログを出してアイコンで代替 */ }
//! }
//! ```
//!
//! # Features
//!
//! - `image`（デフォルト）: PNG / JPEG / GIF / BMP / WebP / TIFF / ICO

mod engine;

#[cfg(feature = "image")]
mod image_provider;

#[cfg(feature = "psd")]
mod psd_provider;

#[cfg(feature = "ffmpeg")]
mod ffmpeg_provider;

#[cfg(feature = "clip")]
mod clip_provider;

#[cfg(feature = "kra")]
mod kra_provider;

#[cfg(feature = "mdp")]
mod mdp_provider;

pub use image::DynamicImage;

// ---- フィットモード ----------------------------------------------------------

/// サムネイルのフィット方法
pub enum FitMode {
    /// アスペクト比を保ったまま size に収める（余白あり）
    Fit,
    /// 短辺を size に合わせて中央クロップ（余白なし、正方形になる）
    Cover,
}

// ---- エラー型 ----------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("decode failed: {0}")]
    Decode(String),

    #[error("process failed: {0}")]
    Process(String),
}

// ---- 結果型 ------------------------------------------------------------------

pub enum ThumbnailResult {
    /// サムネイル生成成功
    Generated(DynamicImage),
    /// 対応しているエンジンがなかった
    Unsupported,
    /// エンジンが担当と判断したが失敗した
    Failed(ThumbnailError),
}

// ---- 公開API ----------------------------------------------------------------

/// サムネイルを生成する
///
/// 有効な feature に対応するエンジンを順に試し、最初に処理できたものの結果を返す。
///
/// - `size`: 物理ピクセル単位の最大サイズ
/// - `fit`: [`FitMode::Fit`] はアスペクト比保持、[`FitMode::Cover`] は中央クロップ
pub fn generate_thumbnail(path: &std::path::Path, size: u32, fit: FitMode) -> ThumbnailResult {
    engine::run(path, size, fit)
}

/// Generate a thumbnail from an already-open source. `path` is used only to
/// select a decoder from its extension; this function never opens it.
pub fn generate_thumbnail_from_reader<R: std::io::Read + std::io::Seek>(
    path: &std::path::Path,
    reader: &mut R,
    size: u32,
    fit: FitMode,
) -> ThumbnailResult {
    engine::run_reader(path, reader, size, fit)
}

#[cfg(test)]
mod reader_tests {
    use std::{io::Cursor, path::Path};

    use super::{FitMode, ThumbnailResult, generate_thumbnail, generate_thumbnail_from_reader};

    #[test]
    fn png_reader_matches_path_fit_output() {
        let path = std::env::temp_dir().join(format!("thumb-reader-{}.png", std::process::id()));
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(13, 7, |x, y| {
            image::Rgba([x as u8, y as u8, (x + y) as u8, 255])
        }));
        source.save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let expected = match generate_thumbnail(&path, 5, FitMode::Fit) {
            ThumbnailResult::Generated(image) => image.to_rgba8(),
            _ => panic!("path decoder should support generated PNG"),
        };
        let actual = match generate_thumbnail_from_reader(
            Path::new("hint.png"),
            &mut Cursor::new(bytes),
            5,
            FitMode::Fit,
        ) {
            ThumbnailResult::Generated(image) => image.to_rgba8(),
            _ => panic!("reader decoder should support generated PNG"),
        };
        assert_eq!(actual, expected);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tiff_reader_matches_path_fit_output() {
        let path = std::env::temp_dir().join(format!("thumb-reader-{}.tiff", std::process::id()));
        let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(17, 9, |x, y| {
            image::Rgb([x as u8, y as u8, (x * y) as u8])
        }));
        source.save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let expected = match generate_thumbnail(&path, 6, FitMode::Fit) {
            ThumbnailResult::Generated(image) => image.to_rgba8(),
            _ => panic!("path decoder should support generated TIFF"),
        };
        let actual = match generate_thumbnail_from_reader(
            Path::new("hint.tiff"),
            &mut Cursor::new(bytes),
            6,
            FitMode::Fit,
        ) {
            ThumbnailResult::Generated(image) => image.to_rgba8(),
            _ => panic!("reader decoder should support generated TIFF"),
        };
        assert_eq!(actual, expected);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(all(
    test,
    feature = "psd",
    feature = "clip",
    feature = "kra",
    feature = "mdp"
))]
mod specialized_reader_parity_tests {
    use std::{
        fs,
        io::{Cursor, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use image::{DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage};

    use super::{FitMode, ThumbnailResult, generate_thumbnail, generate_thumbnail_from_reader};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct FixtureFile(PathBuf);

    impl FixtureFile {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "gpui-filer-thumb-parity-{}-{id}.{extension}",
                std::process::id()
            ));
            fs::write(&path, bytes).expect("write generated thumbnail fixture");
            Self(path)
        }
    }

    impl Drop for FixtureFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn png_fixture() -> Vec<u8> {
        let image = RgbaImage::from_fn(4, 2, |x, y| {
            Rgba([(x * 47) as u8, (y * 119) as u8, 180, 255])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode preview PNG");
        bytes.into_inner()
    }

    fn append_u32_be(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn psd_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"8BPS");
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&3u16.to_be_bytes());
        append_u32_be(&mut out, 2);
        append_u32_be(&mut out, 4);
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes()); // RGB
        append_u32_be(&mut out, 0); // color mode data
        append_u32_be(&mut out, 0); // image resources
        append_u32_be(&mut out, 0); // layer and mask data
        out.extend_from_slice(&0u16.to_be_bytes()); // raw merged image
        // Raw PSD merged data stores one plane per channel.
        out.extend_from_slice(&[20, 70, 120, 170, 220, 30, 80, 130]);
        out.extend_from_slice(&[40, 90, 140, 190, 240, 50, 100, 150]);
        out.extend_from_slice(&[60, 110, 160, 210, 250, 70, 120, 170]);
        out
    }

    fn psb_fixture() -> Vec<u8> {
        let pixels = vec![
            20, 40, 60, 255, 70, 90, 110, 255, 120, 140, 160, 255, 170, 190, 210, 255, 220, 240,
            250, 255, 30, 50, 70, 255, 80, 100, 120, 255, 130, 150, 170, 255,
        ];
        let psd = ag_psd::psd::Psd {
            width: 4.0,
            height: 2.0,
            channels: Some(4.0),
            bits_per_channel: Some(8.0),
            color_mode: Some(ag_psd::psd::ColorMode::Rgb),
            canvas: Some(ag_psd::psd::PixelData {
                width: 4,
                height: 2,
                data: pixels,
            }),
            ..Default::default()
        };
        ag_psd::writer::write_psd(
            &psd,
            &ag_psd::psd::WriteOptions {
                psb: Some(true),
                ..Default::default()
            },
        )
    }

    fn kra_fixture(png: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("mimetype", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"application/x-krita").unwrap();
        archive
            .start_file("preview.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(png).unwrap();
        archive.finish().unwrap().into_inner()
    }

    fn mdp_fixture() -> Vec<u8> {
        let pixels: [u8; 32] = [
            20, 40, 180, 255, 50, 70, 160, 255, 80, 100, 140, 255, 110, 130, 120, 255, 140, 160,
            100, 255, 170, 190, 80, 255, 200, 220, 60, 255, 230, 240, 40, 255,
        ];
        let mut bgra = Vec::with_capacity(pixels.len());
        for pixel in pixels.chunks_exact(4) {
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        let mut compressed = Vec::new();
        {
            let mut encoder =
                flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
            encoder.write_all(&bgra).unwrap();
            encoder.finish().unwrap();
        }
        let xml = b"<Document><Thumb width=\"4\" height=\"2\"/></Document>";
        let mut out = b"mdipack\0".to_vec();
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(xml);
        out.extend_from_slice(b"PAC ");
        out.extend_from_slice(&((8 + 120 + compressed.len()) as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        let mut entry = [0u8; 120];
        entry[..4].copy_from_slice(&(compressed.len() as u32).to_le_bytes());
        entry[4..8].copy_from_slice(&(bgra.len() as u32).to_le_bytes());
        entry[56..61].copy_from_slice(b"thumb");
        out.extend_from_slice(&entry);
        out.extend_from_slice(&compressed);
        out
    }

    fn assert_public_api_parity(extension: &str, bytes: Vec<u8>) {
        let path_file = FixtureFile::new(extension, &bytes);
        for fit in [FitMode::Fit, FitMode::Cover] {
            let fit_name = match fit {
                FitMode::Fit => "Fit",
                FitMode::Cover => "Cover",
            };
            let expected_fit = match fit {
                FitMode::Fit => FitMode::Fit,
                FitMode::Cover => FitMode::Cover,
            };
            let expected = match generate_thumbnail(&path_file.0, 3, expected_fit) {
                ThumbnailResult::Generated(image) => image,
                _ => panic!("path API did not decode .{extension}"),
            };
            let hint = Path::new("reader-input-is-authoritative").with_extension(extension);
            let actual = match generate_thumbnail_from_reader(
                &hint,
                &mut Cursor::new(bytes.as_slice()),
                3,
                fit,
            ) {
                ThumbnailResult::Generated(image) => image,
                _ => panic!("reader API did not decode .{extension}"),
            };
            assert_eq!(
                actual.dimensions(),
                expected.dimensions(),
                ".{extension} {fit_name}"
            );
            assert_eq!(
                actual.to_rgba8().as_raw(),
                expected.to_rgba8().as_raw(),
                ".{extension} {fit_name}"
            );
        }
    }

    fn assert_psd_fixture_decodes_to_expected_pixels(bytes: Vec<u8>) {
        let path_file = FixtureFile::new("psd", &bytes);
        let expected = vec![
            20, 40, 60, 255, 70, 90, 110, 255, 120, 140, 160, 255, 170, 190, 210, 255, 220, 240,
            250, 255, 30, 50, 70, 255, 80, 100, 120, 255, 130, 150, 170, 255,
        ];

        for result in [
            generate_thumbnail(&path_file.0, 4, FitMode::Fit),
            generate_thumbnail_from_reader(
                Path::new("missing-reader-input.psd"),
                &mut Cursor::new(bytes),
                4,
                FitMode::Fit,
            ),
        ] {
            let image = match result {
                ThumbnailResult::Generated(image) => image,
                _ => panic!("PSD fixture should decode to a thumbnail"),
            };
            assert_eq!(image.dimensions(), (4, 2));
            assert_eq!(image.to_rgba8().as_raw(), &expected);
        }
    }

    #[test]
    fn specialized_reader_providers_match_path_api_for_generated_fixtures() {
        let png = png_fixture();
        let psd = psd_fixture();
        assert_public_api_parity("psd", psd.clone());
        assert_psd_fixture_decodes_to_expected_pixels(psd);

        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute("CREATE TABLE CanvasPreview (ImageData BLOB)", [])
            .unwrap();
        connection
            .execute("INSERT INTO CanvasPreview (ImageData) VALUES (?1)", [&png])
            .unwrap();
        let database = connection.serialize(rusqlite::DatabaseName::Main).unwrap();
        let mut clip = b"CLIP fixture prefix".to_vec();
        clip.extend_from_slice(&database);
        assert_public_api_parity("clip", clip);

        let kra = kra_fixture(&png);
        assert_public_api_parity("kra", kra.clone());
        assert_public_api_parity("krz", kra);

        assert_public_api_parity("mdp", mdp_fixture());
    }

    #[cfg(feature = "psd")]
    #[test]
    fn psb_v2_decodes_composite_pixels_through_path_and_reader_apis() {
        let bytes = psb_fixture();
        assert_eq!(&bytes[..6], b"8BPS\0\x02");
        let path_file = FixtureFile::new("psb", &bytes);
        let expected = vec![
            20, 40, 60, 255, 70, 90, 110, 255, 120, 140, 160, 255, 170, 190, 210, 255, 220, 240,
            250, 255, 30, 50, 70, 255, 80, 100, 120, 255, 130, 150, 170, 255,
        ];
        for result in [
            generate_thumbnail(&path_file.0, 4, FitMode::Fit),
            generate_thumbnail_from_reader(
                Path::new("missing-reader-input.psb"),
                &mut Cursor::new(bytes),
                4,
                FitMode::Fit,
            ),
        ] {
            let image = match result {
                ThumbnailResult::Generated(image) => image,
                _ => panic!("PSB fixture should decode to a thumbnail"),
            };
            assert_eq!(image.dimensions(), (4, 2));
            assert_eq!(image.to_rgba8().as_raw(), &expected);
        }

        let mut oversized = psb_fixture();
        oversized[14..18].copy_from_slice(&4096u32.to_be_bytes());
        oversized[18..22].copy_from_slice(&4097u32.to_be_bytes());
        let result = generate_thumbnail_from_reader(
            Path::new("oversized.psb"),
            &mut Cursor::new(oversized),
            4,
            FitMode::Fit,
        );
        match result {
            ThumbnailResult::Failed(crate::ThumbnailError::Decode(message)) => {
                assert!(message.contains("ExceededMemoryLimit"), "{message}");
                assert!(message.contains("available: 67108864"), "{message}");
            }
            _ => panic!("oversized PSB should exceed the custom bitmap budget"),
        }
    }
}
