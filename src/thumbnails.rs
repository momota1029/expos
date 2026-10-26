//! 表示用の縮小画像。元画像の変更を識別し、再起動後もdecode結果を再利用する。
//!
//! 生成そのものは v5 と同じ共有crate `thumb-engine` に任せる。
//! v6 が持つのは「どのサイズで欲しいか」の判定と、メモリ・ディスクの再利用だけ。
use gpui::{App, Global, RenderImage, Window};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, UNIX_EPOCH},
};

/// 無効化の世代。
///
/// 全体の世代と path ごとの世代の **組** で持つ。足し算でまとめると単射でなくなり、
/// 全体 invalidation の前後で同じ値になってしまう。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MediaEpoch {
    global: u64,
    local: u64,
}

/// 要求の識別子。path / decode サイズ / フォルダかどうか / 無効化の世代。
///
/// 世代を key に入れると、invalidate の前後で別の entry になる。GPUI の asset も
/// メモリの LRU も自動で外れ、古い decode が新しいものを上書きしない。
pub type Key = (PathBuf, u32, bool, MediaEpoch);

/// A bounded UI-safe category for a terminal preview miss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewFailureReason {
    Unsupported,
    ProcessingFailed,
    FolderUnavailable,
    FolderNoPreview,
}
const MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MEMORY_ITEMS: usize = 128;
const DISK_BYTES: u64 = 256 * 1024 * 1024;
const DISK_ITEMS: usize = 4096;
const REVALIDATE_AFTER: Duration = Duration::from_secs(30);
/// path ごとに覚える世代の上限。超えたら全体を進めて記録を捨てる。
const MAX_TRACKED_PATHS: usize = 4096;

/// 無効化の世代の置き場。
///
/// source signature（size / mtime）は「同じ値に見える差替え」を検出できない。
/// 世代は明示的な invalidate だけで進むので、signature に映らない変化も
/// 次の要求から作り直せる。
#[derive(Default)]
struct Epochs {
    /// 全体の世代。
    global: u64,
    paths: HashMap<PathBuf, u64>,
}

impl Epochs {
    /// この path の今の世代。O(1) で、filesystem を触らない。
    fn of(&self, path: &Path) -> MediaEpoch {
        MediaEpoch {
            global: self.global,
            local: self.paths.get(path).copied().unwrap_or(0),
        }
    }

    fn bump(&mut self, path: &Path) {
        // 記録が増え続けないよう、上限を超えたら全体を進めて捨てる。
        // 呼び出し側の directory より広く無効化するが、正しさは変わらない。
        if self.paths.len() >= MAX_TRACKED_PATHS {
            self.bump_all();
            return;
        }
        let entry = self.paths.entry(path.to_path_buf()).or_insert(0);
        *entry = entry.wrapping_add(1);
    }

    fn bump_all(&mut self) {
        self.global = self.global.wrapping_add(1);
        // 個別の記録は global に吸収される。持っておく意味がない。
        self.paths.clear();
    }
}

/// 世代は process 全体で一つの置き場にする。
///
/// decode は background thread で走る。そちらからも世代を照合できるよう、
/// `App` の global ではなく static に置く。
fn epochs() -> &'static Mutex<Epochs> {
    static EPOCHS: OnceLock<Mutex<Epochs>> = OnceLock::new();
    EPOCHS.get_or_init(|| Mutex::new(Epochs::default()))
}

/// この path の今の世代。view の projection 照合に使う。
pub fn epoch_of(path: &Path) -> MediaEpoch {
    epochs()
        .lock()
        .map(|state| state.of(path))
        .unwrap_or_default()
}

/// これらの path の表示用画像を作り直す。
///
/// 親 directory の folder preview も一緒に落とす。子の差替えはフォルダ自身の
/// mtime を変えないので、そちらでは気づけない。
pub fn invalidate(paths: &[PathBuf]) {
    let Ok(mut state) = epochs().lock() else {
        return;
    };
    for path in paths {
        state.bump(path);
        if let Some(parent) = path.parent()
            && parent != path
        {
            state.bump(parent);
        }
    }
}

struct Resident {
    used: u64,
    loaded: Instant,
    bytes: Option<usize>,
}
#[derive(Default)]
struct Residents {
    clock: u64,
    entries: HashMap<Key, Resident>,
}
impl Global for Residents {}

impl Residents {
    fn evict(&mut self, protected: &Key) -> Vec<Key> {
        let mut evicted = Vec::new();
        while self.entries.len() > MEMORY_ITEMS
            || self
                .entries
                .values()
                .filter_map(|entry| entry.bytes)
                .sum::<usize>()
                > MEMORY_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .filter(|(key, _)| *key != protected)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            self.entries.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }
}

pub fn begin_frame(cx: &mut App) {
    if !cx.has_global::<Residents>() {
        cx.set_global(Residents::default());
    }
    let pending: Vec<_> = cx
        .global::<Residents>()
        .entries
        .iter()
        .filter(|(_, entry)| entry.bytes.is_none())
        .map(|(key, _)| key.clone())
        .collect();
    for key in pending {
        if let Some(images) = cx.fetch_asset::<crate::directory::Preview>(&key) {
            let resident = cx.global_mut::<Residents>().entries.get_mut(&key).unwrap();
            resident.loaded = Instant::now();
            resident.bytes = Some(
                images
                    .images
                    .iter()
                    .map(|image| image.as_bytes(0).map_or(0, |bytes| bytes.len()))
                    .sum(),
            );
        }
    }
}

/// GPUI assetの重複抑止を使い、結果の保持にはLRU上限を付ける。
pub fn request(
    key: &Key,
    visible: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<crate::directory::PreviewOutput> {
    if !cx.has_global::<Residents>() {
        cx.set_global(Residents::default());
    }
    let mut removed = Vec::new();
    {
        let state = cx.global_mut::<Residents>();
        if state.entries.get(key).is_some_and(|entry| {
            entry.bytes.is_some() && entry.loaded.elapsed() >= REVALIDATE_AFTER
        }) {
            state.entries.remove(key);
            removed.push(key.clone());
        }
        // 先読みだけで大量のdecode待ちを作らない。画面内を先にrequestする。
        let pending = state
            .entries
            .values()
            .filter(|entry| entry.bytes.is_none())
            .count();
        if removed.is_empty()
            && !state.entries.contains_key(key)
            && pending >= if visible { 24 } else { 6 }
        {
            return None;
        }
        state.clock += 1;
        let clock = state.clock;
        state
            .entries
            .entry(key.clone())
            .or_insert(Resident {
                used: clock,
                loaded: Instant::now(),
                bytes: None,
            })
            .used = clock;
    }
    for key in removed {
        cx.remove_asset::<crate::directory::Preview>(&key);
    }
    let images = if visible {
        window.use_asset::<crate::directory::Preview>(key, cx)
    } else {
        window.get_asset::<crate::directory::Preview>(key, cx)
    };
    let evicted = {
        let state = cx.global_mut::<Residents>();
        if let Some(output) = &images {
            let resident = state.entries.get_mut(key).unwrap();
            if resident.bytes.is_none() {
                resident.loaded = Instant::now();
            }
            resident.bytes = Some(
                output
                    .images
                    .iter()
                    .map(|image| image.as_bytes(0).map_or(0, |bytes| bytes.len()))
                    .sum(),
            );
        }
        state.evict(key)
    };
    for key in evicted {
        cx.remove_asset::<crate::directory::Preview>(&key);
    }
    images
}

/// 同時に走らせる decode の本数。
///
/// 2本では可視範囲の項目が順番待ちで詰まる。CPU の並列度に合わせて増やすが、
/// ディスクとメモリを叩きすぎないよう上限を置く。
pub fn decode_slots() -> &'static async_lock::Semaphore {
    static SLOTS: OnceLock<async_lock::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let parallelism = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(4);
        // 半分のコアを使う。最低2本、最大8本。
        let slots = (parallelism / 2).clamp(2, 8);
        async_lock::Semaphore::new(slots)
    })
}

/// サムネイル生成を試すファイルか。`thumb-engine` が担当する全形式を列挙する。
/// v5 の `is_thumbnail_target` と同じ集合に保つ。
/// ここに無い拡張子は生成を試みず、既定アイコンへフォールバックする。
pub fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                // 静止画（image feature）
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tiff" | "tif"
                // PSD（psd feature）
                | "psd" | "psb"
                // CLIP STUDIO PAINT（clip feature）
                | "clip"
                // Krita（kra feature）
                | "kra" | "krz"
                // MediBang Paint Pro（mdp feature）
                | "mdp"
                // 動画（ffmpeg feature）
                | "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "ts" | "m2ts"
            )
        })
}

/// 動画かどうか。ffmpeg のプロセス起動を伴うため、フォルダプレビューの探索では外す。
/// v5 が `is_static_image` で動画を除外しているのと同じ理由。
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "ts" | "m2ts"
            )
        })
}

fn cache_root() -> Option<PathBuf> {
    // 生成の意味が変わったら版を上げる。v1 は縦横比を保つ縮小で、v2 は Cover の正方形。
    // 版を共有すると、古い見た目のキャッシュがそのまま表示されてしまう。
    std::env::var_os("LOCALAPPDATA")
        .map(|root| PathBuf::from(root).join("gpui-filer/v6/thumbnails/v2"))
}

/// ディスクのファイル名。世代も混ぜる。
///
/// 名前の形は `[0-9a-f]{64}.png`（68文字）のままにする。`prune_to` はこの形だけを
/// 削除対象にするので、変えると古い間の掃除が効かなくなる。
fn cache_name(path: &Path, size: u32, epoch: MediaEpoch) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(modified.as_nanos().to_le_bytes());
    hash.update(size.to_le_bytes());
    hash.update(epoch.global.to_le_bytes());
    hash.update(epoch.local.to_le_bytes());
    Some(format!("{:x}.png", hash.finalize()))
}

pub fn load_with_reason(path: &Path, size: u32) -> Result<RenderImage, PreviewFailureReason> {
    let epoch = epoch_of(path);
    load_at_with_reason(path, size, cache_root().as_deref(), epoch)
}

#[cfg(test)]
fn load_at(path: &Path, size: u32, root: Option<&Path>, epoch: MediaEpoch) -> Option<RenderImage> {
    load_at_with_reason(path, size, root, epoch).ok()
}

fn load_at_with_reason(
    path: &Path,
    size: u32,
    root: Option<&Path>,
    epoch: MediaEpoch,
) -> Result<RenderImage, PreviewFailureReason> {
    if !supported(path) {
        return Err(PreviewFailureReason::Unsupported);
    }
    let size = size.clamp(16, 1024);
    let cached = root.and_then(|root| cache_name(path, size, epoch).map(|name| root.join(name)));
    let mut rgba = cached.as_deref().and_then(read_cached);
    if rgba.is_none() {
        rgba = Some(generate(path, size)?);
        // 書く直前に世代を見る。進んでいたら、この名前はもう読まれない。
        // 古い decode が新しい世代のファイルを上書きすることは名前が違うので無い。
        if let Some(cached) = &cached
            && epoch_of(path) == epoch
        {
            // 書けないcacheは表示を失敗させない。renameまでは独立した一時ファイルに書く。
            if let Some(root) = root
                && std::fs::create_dir_all(root).is_ok()
            {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static NEXT_WRITE: AtomicUsize = AtomicUsize::new(0);
                let temporary = cached.with_extension(format!(
                    "{}-{}.tmp",
                    std::process::id(),
                    NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
                ));
                if rgba
                    .as_ref()
                    .unwrap()
                    .save_with_format(&temporary, image::ImageFormat::Png)
                    .is_ok()
                {
                    if std::fs::rename(&temporary, cached).is_err() {
                        let _ = std::fs::remove_file(&temporary);
                    }
                    prune(root);
                }
            }
        }
    }
    let mut rgba = rgba.ok_or(PreviewFailureReason::ProcessingFailed)?;
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(RenderImage::new(vec![image::Frame::new(rgba)]))
}

/// サムネイルを生成する。v5 と同じく `FitMode::Cover` で正方形に切り出す。
///
/// ここで decode の上限を自前で掛けないのが要点。以前は 32MiB のソース除外、
/// 64MiB の割当上限、16,384px の寸法上限を持っていたため、大きい写真ほど
/// `InsufficientMemory` で落ちていた。上限は `thumb-engine` 経由の image 既定に任せる。
fn generate(path: &Path, size: u32) -> Result<image::RgbaImage, PreviewFailureReason> {
    match thumb_engine::generate_thumbnail(path, size, thumb_engine::FitMode::Cover) {
        thumb_engine::ThumbnailResult::Generated(image) => Ok(image.to_rgba8()),
        thumb_engine::ThumbnailResult::Unsupported => {
            // 拡張子が対象外か、動画なのに ffmpeg が PATH に無いかのどちらか。
            log_failure(
                path,
                "no engine produced an image (unsupported or ffmpeg missing)",
            );
            Err(PreviewFailureReason::Unsupported)
        }
        thumb_engine::ThumbnailResult::Failed(error) => {
            log_failure(path, &error.to_string());
            Err(PreviewFailureReason::ProcessingFailed)
        }
    }
}

/// ディスクキャッシュのPNGを読む。既に縮小済みなので寸法上限は掛けない。
fn read_cached(path: &Path) -> Option<image::RgbaImage> {
    image::open(path).ok().map(|image| image.to_rgba8())
}

/// 生成に失敗した理由を残す。既定は無効で、`GPUI_FILER_THUMB_LOG` があるときだけ出す。
/// どのファイルがどの理由で落ちたか分からないと原因を追えないため。
fn log_failure(path: &Path, reason: &str) {
    if std::env::var_os("GPUI_FILER_THUMB_LOG").is_some() {
        eprintln!("[v6-thumb] {} — {}", path.display(), reason);
    }
}

fn prune(root: &Path) {
    // 走査は32回の保存に1回。削除対象はこの版が作るSHA-256名の通常ファイルだけ。
    use std::sync::atomic::{AtomicUsize, Ordering};
    static WRITES: AtomicUsize = AtomicUsize::new(0);
    if !WRITES.fetch_add(1, Ordering::Relaxed).is_multiple_of(32) {
        return;
    }
    prune_to(root, DISK_BYTES, DISK_ITEMS);
}

fn prune_to(root: &Path, max_bytes: u64, max_items: usize) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.len() != 68
                || !name.ends_with(".png")
                || !name.as_bytes()[..64]
                    .iter()
                    .all(|byte| byte.is_ascii_hexdigit())
                || !entry.file_type().ok()?.is_file()
            {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            Some((metadata.modified().ok()?, metadata.len(), entry.path()))
        })
        .collect();
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut bytes: u64 = files.iter().map(|(_, bytes, _)| bytes).sum();
    let mut count = files.len();
    for (_, size, path) in files {
        if bytes <= max_bytes && count <= max_items {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            bytes -= size;
            count -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_cache_evicts_oldest_by_count_and_bytes() {
        let mut cache = Residents::default();
        for index in 0..MEMORY_ITEMS + 1 {
            cache.entries.insert(
                (
                    PathBuf::from(index.to_string()),
                    80,
                    false,
                    MediaEpoch::default(),
                ),
                Resident {
                    used: index as u64,
                    loaded: Instant::now(),
                    bytes: Some(1),
                },
            );
        }
        let newest = (
            PathBuf::from(MEMORY_ITEMS.to_string()),
            80,
            false,
            MediaEpoch::default(),
        );
        assert_eq!(
            cache.evict(&newest),
            vec![(PathBuf::from("0"), 80, false, MediaEpoch::default())]
        );
        cache.entries.get_mut(&newest).unwrap().bytes = Some(MEMORY_BYTES);
        cache.evict(&newest);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key(&newest));
    }

    #[test]
    fn disk_cache_reuses_image_and_invalidates_changed_source() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let source = root.path().join("source.png");
        let epoch = MediaEpoch::default();
        image::RgbaImage::from_pixel(200, 100, image::Rgba([255, 0, 0, 255]))
            .save(&source)
            .unwrap();
        let first = load_at(&source, 80, Some(&cache), epoch).unwrap();
        let cached = cache.join(cache_name(&source, 80, epoch).unwrap());
        let stamp = std::fs::metadata(&cached).unwrap().modified().unwrap();
        let second = load_at(&source, 80, Some(&cache), epoch).unwrap();
        assert_eq!(first.as_bytes(0), second.as_bytes(0));
        assert_eq!(
            stamp,
            std::fs::metadata(&cached).unwrap().modified().unwrap()
        );
        image::RgbaImage::from_pixel(320, 240, image::Rgba([0, 255, 0, 255]))
            .save(&source)
            .unwrap();
        assert_ne!(cached, cache.join(cache_name(&source, 80, epoch).unwrap()));
        let changed = load_at(&source, 80, Some(&cache), epoch).unwrap();
        assert_eq!(&changed.as_bytes(0).unwrap()[..4], &[0, 255, 0, 255]);
    }

    /// B-T11: 同 path / 同 size / 同 mtime の差替え。
    ///
    /// signature は変わらないので、同じ世代では古い disk entry を読む。
    /// 世代を進めたときだけ作り直されることを確かめる。
    #[test]
    fn invalidating_regenerates_a_source_that_changed_without_a_new_signature() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let source = root.path().join("source.bmp");
        let epoch = epoch_of(&source);

        // BMP は同じ寸法なら同じ byte 数になる。差替えを signature から
        // 見えなくするために使う。
        image::RgbImage::from_pixel(64, 32, image::Rgb([255, 0, 0]))
            .save(&source)
            .unwrap();
        let before = std::fs::metadata(&source).unwrap();
        let stamp = before.modified().unwrap();
        let first = load_at(&source, 80, Some(&cache), epoch).unwrap();
        assert_eq!(&first.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        assert!(cache.join(cache_name(&source, 80, epoch).unwrap()).exists());

        // 同じ byte 数の別画像へ差し替え、mtime を元に戻す。
        image::RgbImage::from_pixel(64, 32, image::Rgb([0, 255, 0]))
            .save(&source)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        let after = std::fs::metadata(&source).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(stamp, after.modified().unwrap());

        // 同じ世代では同じファイル名になるので、古い画像が返る。
        let stale = load_at(&source, 80, Some(&cache), epoch).unwrap();
        assert_eq!(&stale.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);

        // 世代を進めると別のファイルになり、作り直される。
        invalidate(std::slice::from_ref(&source));
        let fresh_epoch = epoch_of(&source);
        assert_ne!(fresh_epoch, epoch);
        let fresh = load_at(&source, 80, Some(&cache), fresh_epoch).unwrap();
        assert_eq!(&fresh.as_bytes(0).unwrap()[..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn epochs_advance_per_path_and_globally_and_stay_bounded() {
        let mut epochs = Epochs::default();
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        assert_eq!(epochs.of(&a), MediaEpoch::default());
        epochs.bump(&a);
        assert_ne!(epochs.of(&a), MediaEpoch::default());
        // 触っていない path は変わらない。
        assert_eq!(epochs.of(&b), MediaEpoch::default());

        // 全体を進めると、記録していない path も変わる。
        let (before_a, before_b) = (epochs.of(&a), epochs.of(&b));
        epochs.bump_all();
        assert_ne!(epochs.of(&a), before_a);
        assert_ne!(epochs.of(&b), before_b);
        assert!(epochs.paths.is_empty());

        // 記録は上限を超えて増えない。
        for index in 0..MAX_TRACKED_PATHS * 2 {
            let name = format!("p{index}");
            epochs.bump(Path::new(&name));
        }
        assert!(epochs.paths.len() <= MAX_TRACKED_PATHS);
    }

    #[test]
    fn corrupt_cache_recovers_and_pruning_keeps_unowned_files() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.png");
        image::RgbaImage::from_pixel(20, 10, image::Rgba([1, 2, 3, 255]))
            .save(&source)
            .unwrap();
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        let cached = cache.join(cache_name(&source, 80, MediaEpoch::default()).unwrap());
        std::fs::write(&cached, b"broken").unwrap();
        assert!(load_at(&source, 80, Some(&cache), MediaEpoch::default()).is_some());
        std::fs::write(cache.join("keep.txt"), b"keep").unwrap();
        prune_to(&cache, 0, 0);
        assert!(!cached.exists());
        assert!(cache.join("keep.txt").exists());
    }

    #[test]
    fn supported_follows_the_engine_and_videos_are_kept_out_of_folder_samples() {
        for name in [
            "a.png", "a.JPG", "a.psd", "a.clip", "a.kra", "a.mdp", "a.mp4",
        ] {
            assert!(supported(Path::new(name)), "{name} should be supported");
        }
        assert!(!supported(Path::new("a.txt")));
        assert!(!supported(Path::new("a")));
        assert!(is_video(Path::new("a.MKV")));
        assert!(!is_video(Path::new("a.png")));
    }

    #[test]
    fn wide_jpeg_beyond_the_old_dimension_limit_still_decodes() {
        // 以前は max_image_width/max_image_height = 16,384 で弾いていた。
        // パノラマ写真はこれを超えるので、パノラマがサムネイルにならない原因だった。
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("panorama.jpg");
        image::RgbImage::from_pixel(17_000, 120, image::Rgb([20, 40, 60]))
            .save(&source)
            .unwrap();
        let loaded = load_at(
            &source,
            80,
            Some(&root.path().join("cache")),
            MediaEpoch::default(),
        )
        .unwrap();
        assert_eq!(
            loaded.size(0),
            gpui::size(gpui::DevicePixels(80), gpui::DevicePixels(80))
        );
    }

    /// 同じ修正のもう半分（64MiB の割当上限と 32MiB のソース除外の撤去）を確認する。
    /// デバッグビルドでは decode と Lanczos3 の縮小に1分以上かかるので既定では回さない。
    #[test]
    #[ignore = "重い(約70秒): 64MiB 割当上限の回帰確認"]
    fn large_jpeg_over_the_old_allocation_cap_still_decodes() {
        // 5000x5000 の RGB は展開後 75MB で、以前の 64MiB の上限を超えていた。
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("large.jpg");
        image::RgbImage::from_pixel(5000, 5000, image::Rgb([20, 40, 60]))
            .save(&source)
            .unwrap();
        let loaded = load_at(
            &source,
            80,
            Some(&root.path().join("cache")),
            MediaEpoch::default(),
        )
        .unwrap();
        assert_eq!(
            loaded.size(0),
            gpui::size(gpui::DevicePixels(80), gpui::DevicePixels(80))
        );
    }
}
