//! Path-backed Windows Shell icons for the small This PC projection.
//!
//! Shell/GDI handles and their converted pixels are runtime data. This module owns
//! the bounded request/cache lifecycle; callers keep only a disposable row image.

use crate::{directory::EntrySourceIdentity, thumbnails};
use gpui::RenderImage;
use std::{
    collections::{HashMap, VecDeque},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

const MAX_PENDING: usize = 48;
const MAX_CACHE_ENTRIES: usize = 96;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ICON_PIXELS: u32 = 256;
const SHELL_SLOTS: usize = 2;
const CACHE_FRESHNESS: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    path: PathBuf,
    identity: EntrySourceIdentity,
    pixels: u32,
    invalidation_epoch: thumbnails::MediaEpoch,
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.identity.filesystem_identity().hash(state);
        self.identity.kind.hash(state);
        self.pixels.hash(state);
        self.invalidation_epoch.hash(state);
    }
}

impl Key {
    pub(crate) fn new(
        path: PathBuf,
        identity: EntrySourceIdentity,
        pixels: u32,
        invalidation_epoch: thumbnails::MediaEpoch,
    ) -> Self {
        Self {
            path,
            identity,
            pixels: pixels.clamp(16, MAX_ICON_PIXELS),
            invalidation_epoch,
        }
    }
}

/// Per-view deduplication key; deliberately separate from the shared image-cache key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ViewRequestKey {
    cache_key: Key,
    location_generation: u64,
    presentation_revision: u64,
    source_generation: u64,
}

impl ViewRequestKey {
    pub(crate) fn new(
        cache_key: Key,
        location_generation: u64,
        presentation_revision: u64,
        source_generation: u64,
    ) -> Self {
        Self {
            cache_key,
            location_generation,
            presentation_revision,
            source_generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RequestId(u64);

#[derive(Default)]
struct State {
    cache: HashMap<Key, CacheEntry>,
    lru: VecDeque<Key>,
    cache_bytes: usize,
    pending: HashMap<RequestId, Key>,
    latest_admitted: HashMap<Key, RequestId>,
    failed: HashMap<Key, FailureEntry>,
    failed_lru: VecDeque<Key>,
}

struct CacheEntry {
    image: Arc<RenderImage>,
    // render 側で metadata を再確認せず、admission 時の検証根拠を保持する。
    #[allow(dead_code)]
    signature: SourceSignature,
    admitted_at: Instant,
}

struct FailureEntry {
    // 失敗も成功と同じ source revision に結び付け、期限後は再取得する。
    #[allow(dead_code)]
    signature: SourceSignature,
    admitted_at: Instant,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub(crate) fn cached(key: &Key) -> Option<Arc<RenderImage>> {
    let mut state = state().lock().ok()?;
    cached_at(&mut state, key, Instant::now())
}

fn cached_at(state: &mut State, key: &Key, now: Instant) -> Option<Arc<RenderImage>> {
    if state
        .cache
        .get(key)
        .is_some_and(|entry| now.saturating_duration_since(entry.admitted_at) >= CACHE_FRESHNESS)
    {
        remove_cached(state, key);
        return None;
    }
    let image = state.cache.get(key)?.image.clone();
    if let Some(index) = state.lru.iter().position(|cached| cached == key) {
        state.lru.remove(index);
    }
    state.lru.push_back(key.clone());
    Some(image)
}

pub(crate) fn failed(key: &Key) -> bool {
    state()
        .lock()
        .is_ok_and(|mut state| failed_at(&mut state, key, Instant::now()))
}

fn failed_at(state: &mut State, key: &Key, now: Instant) -> bool {
    if state
        .failed
        .get(key)
        .is_some_and(|entry| now.saturating_duration_since(entry.admitted_at) >= CACHE_FRESHNESS)
    {
        remove_failed(state, key);
        return false;
    }
    state.failed.contains_key(key)
}

/// Reserve one detached lookup. A fast-moving virtual list cannot accumulate an
/// unbounded background queue; separate views keep independent completion callbacks.
pub(crate) fn begin(key: &Key) -> Option<RequestId> {
    let Ok(mut state) = state().lock() else {
        return None;
    };
    if state.pending.len() >= MAX_PENDING {
        return None;
    }
    static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
    let id = RequestId(NEXT_REQUEST.fetch_add(1, Ordering::Relaxed));
    state.pending.insert(id, key.clone());
    Some(id)
}

pub(crate) fn finish(
    request: RequestId,
    key: &Key,
    signature: SourceSignature,
    image: Option<Arc<RenderImage>>,
) -> bool {
    let Ok(mut state) = state().lock() else {
        return false;
    };
    finish_at(&mut state, request, key, signature, image, Instant::now())
}

fn finish_at(
    state: &mut State,
    request: RequestId,
    key: &Key,
    signature: SourceSignature,
    image: Option<Arc<RenderImage>>,
    now: Instant,
) -> bool {
    let Some(pending_key) = state.pending.remove(&request) else {
        return false;
    };
    if pending_key != *key {
        clear_latest_if_idle(state, &pending_key);
        return false;
    }
    if state
        .latest_admitted
        .get(key)
        .is_some_and(|latest| *latest > request)
    {
        clear_latest_if_idle(state, key);
        return false;
    }
    state.latest_admitted.insert(key.clone(), request);
    remove_cached(state, key);
    remove_failed(state, key);
    if let Some(image) = image {
        // Adapters cap dimensions; retain conservative accounting for future decoders.
        let size = key.pixels as usize * key.pixels as usize * 4;
        state.cache_bytes = state.cache_bytes.saturating_add(size);
        state.cache.insert(
            key.clone(),
            CacheEntry {
                image,
                signature,
                admitted_at: now,
            },
        );
        state.lru.push_back(key.clone());
        while state.cache.len() > MAX_CACHE_ENTRIES || state.cache_bytes > MAX_CACHE_BYTES {
            let Some(oldest) = state.lru.pop_front() else {
                break;
            };
            remove_cached(state, &oldest);
        }
    } else {
        state.failed.insert(
            key.clone(),
            FailureEntry {
                signature,
                admitted_at: now,
            },
        );
        state.failed_lru.push_back(key.clone());
        while state.failed_lru.len() > MAX_CACHE_ENTRIES {
            if let Some(oldest) = state.failed_lru.pop_front() {
                state.failed.remove(&oldest);
            }
        }
    }
    clear_latest_if_idle(state, key);
    true
}

fn clear_latest_if_idle(state: &mut State, key: &Key) {
    if !state.pending.values().any(|pending_key| pending_key == key) {
        state.latest_admitted.remove(key);
    }
}

fn remove_cached(state: &mut State, key: &Key) {
    if state.cache.remove(key).is_some() {
        let size = key.pixels as usize * key.pixels as usize * 4;
        state.cache_bytes = state.cache_bytes.saturating_sub(size);
    }
    if let Some(index) = state.lru.iter().position(|cached| cached == key) {
        state.lru.remove(index);
    }
}

fn remove_failed(state: &mut State, key: &Key) {
    state.failed.remove(key);
    if let Some(index) = state.failed_lru.iter().position(|failed| failed == key) {
        state.failed_lru.remove(index);
    }
}

pub(crate) fn cancel(request: RequestId) {
    if let Ok(mut state) = state().lock() {
        if let Some(key) = state.pending.remove(&request) {
            clear_latest_if_idle(&mut state, &key);
        }
    }
}

/// Capture the filesystem identity away from the render/input path. Reparse paths
/// are excluded through `matches_path` before the identity is handed to a row.
pub(crate) async fn capture_identity(path: &Path) -> Option<EntrySourceIdentity> {
    let _permit = shell_slots().acquire().await;
    let identity = EntrySourceIdentity::from_path(path, crate::directory::EntryKind::Folder)?;
    identity.matches_path(path).then_some(identity)
}

pub(crate) struct LoadedIcon {
    pub(crate) image: Arc<RenderImage>,
    pub(crate) signature: SourceSignature,
}

pub(crate) enum LoadResult {
    Loaded(LoadedIcon),
    Failed(SourceSignature),
    Stale,
}

pub(crate) async fn load(key: &Key) -> LoadResult {
    let _permit = shell_slots().acquire().await;
    if !key.identity.matches_path(&key.path) {
        return LoadResult::Stale;
    }
    let Some(before) = SourceSignature::read(&key.path) else {
        return LoadResult::Stale;
    };
    let rgba = decode_shell_icon(&key.path, key.pixels);
    let Some(after) = SourceSignature::read(&key.path) else {
        return LoadResult::Stale;
    };
    if !key.identity.matches_path(&key.path) || after != before {
        return LoadResult::Stale;
    }
    let Some(rgba) = rgba else {
        return LoadResult::Failed(after);
    };
    LoadResult::Loaded(LoadedIcon {
        image: Arc::new(render_image(rgba.clone())),
        signature: after,
    })
}

fn render_image(rgba: image::RgbaImage) -> RenderImage {
    RenderImage::new(vec![image::Frame::new(rgba)])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceSignature {
    pub(crate) len: u64,
    pub(crate) modified_ns: u128,
}

impl SourceSignature {
    fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified_ns = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(Self {
            len: metadata.len(),
            modified_ns,
        })
    }
}

fn shell_slots() -> &'static async_lock::Semaphore {
    static SLOTS: OnceLock<async_lock::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| async_lock::Semaphore::new(SHELL_SLOTS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::EntryKind;

    fn key() -> Key {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shell-icon-source.bin");
        fs::write(&path, b"source").unwrap();
        let identity = EntrySourceIdentity::from_path(&path, EntryKind::File).unwrap();
        Key::new(path, identity, 32, thumbnails::MediaEpoch::default())
    }

    fn image(color: [u8; 4]) -> Arc<RenderImage> {
        Arc::new(render_image(image::RgbaImage::from_pixel(
            16,
            16,
            image::Rgba(color),
        )))
    }

    #[test]
    fn expired_success_is_reloadable_and_fresh_signature_replaces_it() {
        let key = key();
        let mut state = State::default();
        let accepted_at = Instant::now();
        let old_signature = SourceSignature {
            len: 6,
            modified_ns: 10,
        };
        let old_image = image([1, 2, 3, 255]);
        let old_request = RequestId(1);
        state.pending.insert(old_request, key.clone());
        assert!(finish_at(
            &mut state,
            old_request,
            &key,
            old_signature,
            Some(old_image.clone()),
            accepted_at,
        ));
        assert!(Arc::ptr_eq(
            &cached_at(
                &mut state,
                &key,
                accepted_at + CACHE_FRESHNESS - Duration::from_nanos(1)
            )
            .unwrap(),
            &old_image
        ));

        let expired_at = accepted_at + CACHE_FRESHNESS;
        assert!(cached_at(&mut state, &key, expired_at).is_none());
        assert_eq!(state.cache_bytes, 0);

        let new_signature = SourceSignature {
            len: 7,
            modified_ns: 20,
        };
        let new_image = image([4, 5, 6, 255]);
        let new_request = RequestId(2);
        state.pending.insert(new_request, key.clone());
        assert!(finish_at(
            &mut state,
            new_request,
            &key,
            new_signature,
            Some(new_image.clone()),
            expired_at,
        ));
        assert_eq!(state.cache.get(&key).unwrap().signature, new_signature);
        assert!(Arc::ptr_eq(
            &cached_at(&mut state, &key, expired_at + Duration::from_secs(1)).unwrap(),
            &new_image
        ));
    }

    #[test]
    fn expired_failure_is_retryable_and_fresh_success_replaces_it() {
        let key = key();
        let mut state = State::default();
        let accepted_at = Instant::now();
        let old_signature = SourceSignature {
            len: 6,
            modified_ns: 10,
        };
        let failed_request = RequestId(3);
        state.pending.insert(failed_request, key.clone());
        assert!(finish_at(
            &mut state,
            failed_request,
            &key,
            old_signature,
            None,
            accepted_at,
        ));
        assert!(failed_at(
            &mut state,
            &key,
            accepted_at + CACHE_FRESHNESS - Duration::from_nanos(1)
        ));

        let expired_at = accepted_at + CACHE_FRESHNESS;
        assert!(!failed_at(&mut state, &key, expired_at));
        assert!(state.failed.is_empty());

        let new_signature = SourceSignature {
            len: 7,
            modified_ns: 20,
        };
        let fresh_request = RequestId(4);
        state.pending.insert(fresh_request, key.clone());
        assert!(finish_at(
            &mut state,
            fresh_request,
            &key,
            new_signature,
            Some(image([9, 8, 7, 255])),
            expired_at,
        ));
        assert!(!failed_at(&mut state, &key, expired_at));
        assert_eq!(state.cache.get(&key).unwrap().signature, new_signature);
    }

    #[test]
    fn late_success_cannot_replace_a_newer_success_for_the_same_key() {
        let key = key();
        let mut state = State::default();
        let older = RequestId(10);
        let newer = RequestId(11);
        state.pending.insert(older, key.clone());
        state.pending.insert(newer, key.clone());
        let newer_image = image([10, 20, 30, 255]);
        let now = Instant::now();

        assert!(finish_at(
            &mut state,
            newer,
            &key,
            SourceSignature {
                len: 20,
                modified_ns: 20,
            },
            Some(newer_image.clone()),
            now,
        ));
        assert!(!finish_at(
            &mut state,
            older,
            &key,
            SourceSignature {
                len: 10,
                modified_ns: 10,
            },
            Some(image([1, 2, 3, 255])),
            now + Duration::from_secs(1),
        ));

        assert!(Arc::ptr_eq(
            &cached_at(&mut state, &key, now + Duration::from_secs(1)).unwrap(),
            &newer_image
        ));
        assert!(state.pending.is_empty());
        assert!(state.latest_admitted.is_empty());
    }

    #[test]
    fn late_failure_cannot_replace_a_newer_success_for_the_same_key() {
        let key = key();
        let mut state = State::default();
        let older = RequestId(20);
        let newer = RequestId(21);
        state.pending.insert(older, key.clone());
        state.pending.insert(newer, key.clone());
        let newer_image = image([40, 50, 60, 255]);
        let now = Instant::now();

        assert!(finish_at(
            &mut state,
            newer,
            &key,
            SourceSignature {
                len: 20,
                modified_ns: 20,
            },
            Some(newer_image.clone()),
            now,
        ));
        assert!(!finish_at(
            &mut state,
            older,
            &key,
            SourceSignature {
                len: 10,
                modified_ns: 10,
            },
            None,
            now + Duration::from_secs(1),
        ));

        assert!(Arc::ptr_eq(
            &cached_at(&mut state, &key, now + Duration::from_secs(1)).unwrap(),
            &newer_image
        ));
        assert!(!failed_at(&mut state, &key, now + Duration::from_secs(1)));
        assert!(state.pending.is_empty());
        assert!(state.latest_admitted.is_empty());
    }
}

#[cfg(windows)]
fn decode_shell_icon(path: &Path, pixels: u32) -> Option<image::RgbaImage> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Graphics::Gdi::{
                BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
                DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HGDIOBJ, SelectObject,
            },
            UI::{
                Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW},
                WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx},
            },
        },
        core::PCWSTR,
    };

    struct Icon(windows::Win32::UI::WindowsAndMessaging::HICON);
    impl Drop for Icon {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }
    struct Dc(windows::Win32::Graphics::Gdi::HDC);
    impl Drop for Dc {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
    struct Bitmap(HBITMAP);
    impl Drop for Bitmap {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0.0));
            }
        }
    }

    let pixels = pixels.clamp(16, MAX_ICON_PIXELS);
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut info = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if result == 0 || info.hIcon.0.is_null() {
        return None;
    }
    let icon = Icon(info.hIcon);
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return None;
    }
    let dc = Dc(dc);
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: pixels as i32,
        biHeight: -(pixels as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let bitmap_info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let Ok(bitmap) =
        (unsafe { CreateDIBSection(dc.0, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0) })
    else {
        return None;
    };
    if bitmap.0.is_null() || bits.is_null() {
        return None;
    }
    let bitmap = Bitmap(bitmap);
    let previous = unsafe { SelectObject(dc.0, HGDIOBJ(bitmap.0.0)) };
    if previous.0.is_null() {
        return None;
    }
    let drawn = unsafe {
        DrawIconEx(
            dc.0,
            0,
            0,
            icon.0,
            pixels as i32,
            pixels as i32,
            0,
            None,
            DI_NORMAL,
        )
    };
    unsafe {
        SelectObject(dc.0, previous);
    }
    if drawn.is_err() {
        return None;
    }
    let count = (pixels * pixels) as usize;
    let bgra = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), count * 4) };
    let mut rgba = image::RgbaImage::new(pixels, pixels);
    for (source, destination) in bgra.chunks_exact(4).zip(rgba.pixels_mut()) {
        // GDI の DIB は BGRA で、RenderImage に渡す既存経路と同じ並び。ここで
        // R/B を入れ替えると Shell 関連付けアイコンの赤と青が反転してしまう。
        *destination = image::Rgba([source[0], source[1], source[2], source[3]]);
    }
    Some(rgba)
}

#[cfg(not(windows))]
fn decode_shell_icon(_path: &Path, _pixels: u32) -> Option<image::RgbaImage> {
    None
}
