//! Shell-owned Quick Preview state and detached image/text/video/folder decode adapters.

use crate::{
    directory::EntrySourceIdentity, entry_identity::EntryKey,
    filesystem_identity::FilesystemIdentity, thumbnails::MediaEpoch,
};
use gpui::{RenderImage, ScrollHandle};
use gpui_filer_resources::IconId;
use image::ImageDecoder as _;
use std::{
    collections::VecDeque,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TEXT_READ_LIMIT: usize = 64 * 1024;
const TEXT_OUTPUT_LIMIT: usize = 192 * 1024;
const VIDEO_WALL_BUDGET: Duration = Duration::from_secs(8);
const VIDEO_CLEANUP_RESERVE: Duration = Duration::from_secs(1);
const VIDEO_STDOUT_LIMIT: usize = 8 * 1024 * 1024;
const VIDEO_METADATA_STDOUT_LIMIT: usize = 256;
const VIDEO_MAX_DIMENSION: u32 = 2048;
const VIDEO_MAX_PIXELS: u64 = 4 * 1024 * 1024;
const FOLDER_IMAGE_LIMIT: usize = 3;
const FOLDER_IMAGE_DIMENSION: u32 = 512;
const FOLDER_SCAN_BATCH: usize = 32;
const FOLDER_SCAN_YIELD: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DecodeSpec {
    pub max_dimension: u32,
}

impl Default for DecodeSpec {
    fn default() -> Self {
        Self {
            max_dimension: 2048,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub tab_id: usize,
    pub location_generation: u64,
    pub key: EntryKey,
    pub path: PathBuf,
    pub source_identity: Option<EntrySourceIdentity>,
    /// Static catalog art for an ordinary file with no preview decoder.
    pub fallback_icon: Option<IconId>,
    pub epoch: MediaEpoch,
    pub spec: DecodeSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub target: Target,
    pub target_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPreview {
    pub text: String,
    pub encoding: &'static str,
    pub lossy: bool,
    pub truncated: bool,
}

#[derive(Clone)]
pub struct VideoPreview {
    pub frame: Option<Arc<RenderImage>>,
    pub duration_seconds: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: u64,
}

#[derive(Clone)]
pub enum Content {
    Loading,
    Image(Arc<RenderImage>),
    FolderPreview(Arc<Vec<Arc<RenderImage>>>),
    Text(TextPreview),
    Video(VideoPreview),
    Generic(IconId),
    Failed,
}

/// Filesystem pins outlive detached decoding until the Shell commits or drops
/// the result. The Shell's generation/epoch gate remains the publication gate.
pub struct Decoded {
    pub content: Content,
    _source_lease: SourceLease,
}

#[derive(Default)]
struct SourceLease {
    _pins: Vec<std::fs::File>,
    epochs: Vec<(PathBuf, MediaEpoch)>,
}

impl Decoded {
    fn without_lease(content: Content) -> Self {
        Self {
            content,
            _source_lease: SourceLease::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_envelope(content: Content) -> Self {
        Self::without_lease(content)
    }

    fn with_epoch(content: Content, path: PathBuf, epoch: MediaEpoch) -> Self {
        Self {
            content,
            _source_lease: SourceLease {
                _pins: Vec::new(),
                epochs: vec![(path, epoch)],
            },
        }
    }

    fn with_pins_and_epochs(
        content: Content,
        pins: Vec<std::fs::File>,
        epochs: Vec<(PathBuf, MediaEpoch)>,
    ) -> Self {
        Self {
            content,
            _source_lease: SourceLease {
                _pins: pins,
                epochs,
            },
        }
    }

    pub fn source_epochs_are_current(&self) -> bool {
        self._source_lease
            .epochs
            .iter()
            .all(|(path, epoch)| crate::thumbnails::epoch_of(path) == *epoch)
    }
}

pub struct Session {
    target: Option<Target>,
    target_generation: u64,
    attempt_cancellation: AttemptCancellation,
    content: Option<Content>,
    text_scroll: ScrollHandle,
}

#[derive(Clone)]
pub struct AttemptCancellation(Arc<AtomicBool>);

impl AttemptCancellation {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            target: None,
            target_generation: 0,
            attempt_cancellation: AttemptCancellation::new(),
            content: None,
            text_scroll: ScrollHandle::new(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.attempt_cancellation.cancel();
    }
}

impl Session {
    pub fn is_open(&self) -> bool {
        self.target.is_some()
    }
    pub fn content(&self) -> Option<Content> {
        self.content.clone()
    }
    pub fn text_scroll(&self) -> &ScrollHandle {
        &self.text_scroll
    }
    pub fn target_generation(&self) -> u64 {
        self.target_generation
    }
    pub fn attempt_cancellation(&self) -> AttemptCancellation {
        self.attempt_cancellation.clone()
    }

    /// 同じ immutable target の重複 notify では decode を再要求しない。
    pub fn open_or_update(&mut self, target: Target) -> bool {
        if self.target.as_ref() == Some(&target) {
            return false;
        }
        self.attempt_cancellation.cancel();
        self.attempt_cancellation = AttemptCancellation::new();
        self.target = Some(target);
        self.target_generation = self.target_generation.wrapping_add(1);
        self.content = Some(Content::Loading);
        self.text_scroll = ScrollHandle::new();
        true
    }

    pub fn close(&mut self) {
        self.attempt_cancellation.cancel();
        self.target = None;
        self.target_generation = self.target_generation.wrapping_add(1);
        self.content = None;
        self.text_scroll = ScrollHandle::new();
    }

    pub fn accepts(
        &self,
        completion: &Completion,
        active_tab: usize,
        location_generation: u64,
        current_epoch: MediaEpoch,
    ) -> bool {
        self.target.as_ref() == Some(&completion.target)
            && completion.target.tab_id == active_tab
            && completion.target.location_generation == location_generation
            && completion.target_generation == self.target_generation
            && completion.target.epoch == current_epoch
    }

    pub fn commit(
        &mut self,
        completion: Completion,
        content: Content,
        active_tab: usize,
        location_generation: u64,
        current_epoch: MediaEpoch,
    ) -> bool {
        if !self.accepts(&completion, active_tab, location_generation, current_epoch) {
            return false;
        }
        self.content = Some(content);
        true
    }

    /// The decoded envelope remains alive through the owner gates and drops
    /// only after this commit attempt has accepted or rejected the result.
    #[cfg(test)]
    pub fn commit_decoded(
        &mut self,
        completion: Completion,
        decoded: Decoded,
        active_tab: usize,
        location_generation: u64,
        current_epoch: MediaEpoch,
    ) -> bool {
        let source_epochs_current = decoded.source_epochs_are_current();
        source_epochs_current
            && self.commit(
                completion,
                decoded.content.clone(),
                active_tab,
                location_generation,
                current_epoch,
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceStamp {
    length: u64,
    modified_ns: Option<u128>,
    created_ns: Option<u128>,
}

impl SourceStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let timestamp_ns = |time: std::io::Result<SystemTime>| {
            time.ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
        };
        Self {
            length: metadata.len(),
            modified_ns: timestamp_ns(metadata.modified()),
            created_ns: timestamp_ns(metadata.created()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    entry: EntryKey,
    path: PathBuf,
    spec: DecodeSpec,
    epoch: MediaEpoch,
    source: SourceStamp,
    identity: FilesystemIdentity,
}

struct Cache {
    entries: VecDeque<(CacheKey, Arc<RenderImage>)>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            entries: VecDeque::new(),
        })
    })
}

/// Decode is deliberately separate from the thumbnail cache and uses Fit, not Cover.
pub async fn decode(
    target: Target,
    cancellation: AttemptCancellation,
    background: gpui::BackgroundExecutor,
) -> Option<Decoded> {
    if cancellation.is_cancelled() {
        return None;
    }
    if let Some(icon) = target.fallback_icon {
        let ordinary_file = matches!(
            &target.key,
            EntryKey::Path {
                kind: crate::directory::EntryKind::File,
                ..
            }
        ) && target
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity.kind == crate::directory::EntryKind::File);
        if ordinary_file
            && !is_text_preview_target(&target.path)
            && !crate::thumbnails::supported(&target.path)
            && !crate::thumbnails::is_video(&target.path)
        {
            return Some(Decoded::without_lease(Content::Generic(icon)));
        }
    }
    if matches!(
        &target.key,
        EntryKey::Path {
            kind: crate::directory::EntryKind::Folder,
            ..
        }
    ) {
        if !target
            .source_identity
            .as_ref()
            .is_some_and(|identity| identity.kind == crate::directory::EntryKind::Folder)
        {
            return Some(Decoded::without_lease(Content::Failed));
        }
        return decode_folder(&target, &cancellation, &background).await;
    }
    if is_text_preview_target(&target.path) {
        if crate::thumbnails::epoch_of(&target.path) != target.epoch {
            return None;
        }
        let result = read_text_preview(&target);
        if crate::thumbnails::epoch_of(&target.path) != target.epoch {
            return None;
        }
        return Some(Decoded::without_lease(
            result.map_or(Content::Failed, Content::Text),
        ));
    }
    if crate::thumbnails::is_video(&target.path) {
        return decode_video(&target).await;
    }
    decode_image(target).await
}

async fn decode_folder(
    target: &Target,
    cancellation: &AttemptCancellation,
    background: &gpui::BackgroundExecutor,
) -> Option<Decoded> {
    #[cfg(target_os = "windows")]
    {
        decode_folder_windows(target, cancellation, background).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 他OSでは現在 no-follow / no-replacement の folder pin が無いため、空のfallbackにする。
        let _ = (target, cancellation, background);
        Some(Decoded::without_lease(Content::FolderPreview(Arc::new(
            Vec::new(),
        ))))
    }
}

#[cfg(target_os = "windows")]
async fn decode_folder_windows(
    target: &Target,
    cancellation: &AttemptCancellation,
    background: &gpui::BackgroundExecutor,
) -> Option<Decoded> {
    if cancellation.is_cancelled() {
        return None;
    }
    let Some(expected) = target
        .source_identity
        .as_ref()
        .filter(|identity| identity.kind == crate::directory::EntryKind::Folder)
    else {
        return Some(Decoded::without_lease(Content::Failed));
    };
    if crate::thumbnails::epoch_of(&target.path) != target.epoch {
        return None;
    }
    let Some((parent_pin, route_pins, parent_stamp)) = open_pinned_folder(target, expected) else {
        if cancellation.is_cancelled() {
            return None;
        }
        return folder_open_failure_if_current(target);
    };
    if cancellation.is_cancelled()
        || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
    {
        return None;
    }

    struct AcceptedChild {
        path: PathBuf,
        identity: EntrySourceIdentity,
        stamp: SourceStamp,
        epoch: MediaEpoch,
        pin: std::fs::File,
        image: Arc<RenderImage>,
    }

    let mut accepted = Vec::with_capacity(FOLDER_IMAGE_LIMIT);
    let entries = match std::fs::read_dir(&target.path) {
        Ok(entries) => entries,
        Err(_) => {
            if cancellation.is_cancelled()
                || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
            {
                return None;
            }
            return Some(Decoded::with_pins_and_epochs(
                Content::Failed,
                std::iter::once(parent_pin).chain(route_pins).collect(),
                vec![(target.path.clone(), target.epoch)],
            ));
        }
    };
    let mut entries = entries;
    let mut finished = false;
    loop {
        if cancellation.is_cancelled()
            || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
        {
            return None;
        }
        let permit = crate::thumbnails::decode_slots().acquire().await;
        if cancellation.is_cancelled()
            || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
        {
            drop(permit);
            return None;
        }
        for _ in 0..FOLDER_SCAN_BATCH {
            if cancellation.is_cancelled() {
                return None;
            }
            if accepted.len() == FOLDER_IMAGE_LIMIT {
                finished = true;
                break;
            }
            let Some(entry) = entries.next() else {
                finished = true;
                break;
            };
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file()
                || file_type.is_symlink()
                || !crate::thumbnails::supported(&path)
                || crate::thumbnails::is_video(&path)
            {
                continue;
            }
            let Some(child_identity) =
                EntrySourceIdentity::from_path(&path, crate::directory::EntryKind::File)
            else {
                continue;
            };
            let Some((child_pin, child_stamp)) = open_pinned_child(&path, &child_identity) else {
                continue;
            };
            let child_epoch = crate::thumbnails::epoch_of(&path);
            let mut child_pin = child_pin;
            let decoded = match thumb_engine::generate_thumbnail_from_reader(
                &path,
                &mut child_pin,
                FOLDER_IMAGE_DIMENSION,
                thumb_engine::FitMode::Fit,
            ) {
                thumb_engine::ThumbnailResult::Generated(image) => {
                    Some(Arc::new(render_image(image.to_rgba8())))
                }
                _ => None,
            };
            if cancellation.is_cancelled()
                || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
            {
                return None;
            }
            if child_pin_is_current(
                &path,
                &child_identity,
                &child_pin,
                &child_stamp,
                child_epoch,
            ) && let Some(image) = decoded
            {
                accepted.push(AcceptedChild {
                    path,
                    identity: child_identity,
                    stamp: child_stamp,
                    epoch: child_epoch,
                    pin: child_pin,
                    image,
                });
            }
        }
        drop(permit);
        if !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp) {
            return None;
        }
        if finished {
            break;
        }
        if cancellation.is_cancelled() {
            return None;
        }
        background.timer(FOLDER_SCAN_YIELD).await;
        if cancellation.is_cancelled()
            || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
        {
            return None;
        }
    }
    if cancellation.is_cancelled()
        || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
    {
        return None;
    }
    let mut images = Vec::with_capacity(accepted.len());
    for child in &accepted {
        if child_pin_is_current(
            &child.path,
            &child.identity,
            &child.pin,
            &child.stamp,
            child.epoch,
        ) {
            images.push(child.image.clone());
        }
    }
    if cancellation.is_cancelled()
        || !folder_pin_is_current(target, expected, &parent_pin, &parent_stamp)
    {
        return None;
    }
    let mut epochs = vec![(target.path.clone(), target.epoch)];
    epochs.extend(
        accepted
            .iter()
            .map(|child| (child.path.clone(), child.epoch)),
    );
    let mut pins = Vec::with_capacity(1 + route_pins.len() + accepted.len());
    pins.push(parent_pin);
    pins.extend(route_pins);
    pins.extend(accepted.into_iter().map(|child| child.pin));
    Some(Decoded::with_pins_and_epochs(
        Content::FolderPreview(Arc::new(images)),
        pins,
        epochs,
    ))
}

fn folder_open_failure_if_current(target: &Target) -> Option<Decoded> {
    (crate::thumbnails::epoch_of(&target.path) == target.epoch)
        .then(|| Decoded::with_epoch(Content::Failed, target.path.clone(), target.epoch))
}

#[cfg(target_os = "windows")]
fn open_pinned_folder(
    target: &Target,
    expected: &EntrySourceIdentity,
) -> Option<(std::fs::File, Vec<std::fs::File>, SourceStamp)> {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // 先に root から順に route を pin し、後続の leaf 検証まで差し替え窓を作らない。
    let route_pins = open_folder_route_leases(&target.path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        // delete shareを許さず、enumeration中のfolder path置換を防ぐ。
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(&target.path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || !expected.matches_path(&target.path)
        || filesystem_identity_from_handle(&file)? != expected.filesystem_identity()
        || crate::thumbnails::epoch_of(&target.path) != target.epoch
    {
        return None;
    }
    let stamp = SourceStamp::from_metadata(&metadata);
    Some((file, route_pins, stamp))
}

#[cfg(target_os = "windows")]
fn open_folder_route_leases(path: &Path) -> Option<Vec<std::fs::File>> {
    let mut retained_handles = 0;
    open_folder_route_leases_at_depth(path, 0, &mut retained_handles)
}

#[cfg(target_os = "windows")]
fn open_folder_route_leases_at_depth(
    path: &Path,
    depth: usize,
    retained_handles: &mut usize,
) -> Option<Vec<std::fs::File>> {
    const MAX_REPARSE_ROUTE_DEPTH: usize = 32;
    const MAX_ROUTE_COMPONENTS: usize = 256;
    if depth > MAX_REPARSE_ROUTE_DEPTH {
        return None;
    }

    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    let mut prefixes = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        prefixes.push(prefix.clone());
        if prefixes.len() > MAX_ROUTE_COMPONENTS {
            return None;
        }
    }

    let mut pins = Vec::new();
    for prefix in prefixes {
        // 先に no-follow handle を write/delete 非共有で取る。reparse data を
        // 変更できる既存 writer があれば、この open が失敗して fail closed になる。
        let no_follow = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&prefix)
            .ok()?;
        let is_reparse =
            no_follow.metadata().ok()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
        let reparse_target = if is_reparse {
            Some(reparse_target_from_handle(&no_follow, &prefix)?)
        } else {
            None
        };

        let resolved = std::fs::OpenOptions::new()
            .read(true)
            // Route directory が置換されないよう delete share を許さない。
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(&prefix)
            .ok()?;
        if !resolved.metadata().ok()?.is_dir() {
            return None;
        }
        if is_reparse {
            // Link object の直下 target route を辿り、隠れた nested reparse chain も pin する。
            let target_route_pins = open_folder_route_leases_at_depth(
                reparse_target.as_deref()?,
                depth + 1,
                retained_handles,
            )?;
            pins.extend(target_route_pins);
            // Resolved 側の親 route も pin し、junction target の祖先差し替えを防ぐ。
            let resolved_route = final_path_from_handle(&resolved)?;
            let target_route_pins = open_resolved_route_leases(&resolved_route, retained_handles)?;
            if final_path_from_handle(&resolved)? != resolved_route {
                return None;
            }
            pins.extend(target_route_pins);
            // Stable な junction / symlink object も route identity の一部として pin する。
            retain_route_pin(&mut pins, no_follow, retained_handles)?;
        }
        retain_route_pin(&mut pins, resolved, retained_handles)?;
    }
    Some(pins)
}

#[cfg(target_os = "windows")]
fn retain_route_pin(
    pins: &mut Vec<std::fs::File>,
    file: std::fs::File,
    retained_handles: &mut usize,
) -> Option<()> {
    const MAX_RETAINED_ROUTE_HANDLES: usize = 512;
    if *retained_handles >= MAX_RETAINED_ROUTE_HANDLES {
        return None;
    }
    *retained_handles += 1;
    pins.push(file);
    Some(())
}

#[cfg(target_os = "windows")]
fn reparse_target_from_handle(file: &std::fs::File, link_path: &Path) -> Option<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};

    const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
    const SYMLINK_FLAG_RELATIVE: u32 = 1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            device: *mut std::ffi::c_void,
            control_code: u32,
            input: *const std::ffi::c_void,
            input_size: u32,
            output: *mut std::ffi::c_void,
            output_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }

    let mut bytes = vec![0u8; 16 * 1024];
    let mut bytes_returned = 0u32;
    let succeeded = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if succeeded == 0 || bytes_returned < 16 {
        return None;
    }
    let tag = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
    let data_length = usize::from(u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?));
    let (path_buffer_start, substitute_offset, substitute_length, relative): (
        usize,
        usize,
        usize,
        bool,
    ) = match tag {
        IO_REPARSE_TAG_MOUNT_POINT => (
            16,
            usize::from(u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?)),
            usize::from(u16::from_le_bytes(bytes.get(10..12)?.try_into().ok()?)),
            false,
        ),
        IO_REPARSE_TAG_SYMLINK => (
            20,
            usize::from(u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?)),
            usize::from(u16::from_le_bytes(bytes.get(10..12)?.try_into().ok()?)),
            u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?) & SYMLINK_FLAG_RELATIVE != 0,
        ),
        _ => return None,
    };
    let data_end = 8usize.checked_add(data_length)?;
    if data_end > bytes_returned as usize || substitute_length == 0 || substitute_length % 2 != 0 {
        return None;
    }
    let start = path_buffer_start.checked_add(substitute_offset)?;
    let end = start.checked_add(substitute_length)?;
    if end > data_end {
        return None;
    }
    let target_units = bytes
        .get(start..end)?
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    if target_units.contains(&0) {
        return None;
    }
    let target = if relative {
        link_path
            .parent()?
            .join(std::ffi::OsString::from_wide(&target_units))
    } else {
        let nt_prefix = "\\??\\".encode_utf16().collect::<Vec<_>>();
        let unc_prefix = "\\??\\UNC\\".encode_utf16().collect::<Vec<_>>();
        let volume_prefix = "\\??\\Volume{".encode_utf16().collect::<Vec<_>>();
        if target_units
            .get(..unc_prefix.len())
            .is_some_and(|prefix| wide_prefix_eq_ascii(prefix, &unc_prefix))
        {
            let mut dos_unc = std::ffi::OsString::from("\\\\");
            dos_unc.push(std::ffi::OsString::from_wide(
                &target_units[unc_prefix.len()..],
            ));
            PathBuf::from(dos_unc)
        } else if target_units
            .get(..volume_prefix.len())
            .is_some_and(|prefix| wide_prefix_eq_ascii(prefix, &volume_prefix))
        {
            let mut volume_path = std::ffi::OsString::from("\\\\?\\");
            volume_path.push(std::ffi::OsString::from_wide(
                &target_units[nt_prefix.len()..],
            ));
            PathBuf::from(volume_path)
        } else if target_units
            .get(..nt_prefix.len())
            .is_some_and(|prefix| wide_prefix_eq_ascii(prefix, &nt_prefix))
        {
            PathBuf::from(std::ffi::OsString::from_wide(
                &target_units[nt_prefix.len()..],
            ))
        } else if target_units.starts_with(&"\\\\?\\".encode_utf16().collect::<Vec<_>>())
            || target_units.get(1) == Some(&(b':' as u16))
        {
            PathBuf::from(std::ffi::OsString::from_wide(&target_units))
        } else {
            return None;
        }
    };
    target.is_absolute().then_some(target)
}

#[cfg(target_os = "windows")]
fn wide_prefix_eq_ascii(prefix: &[u16], expected: &[u16]) -> bool {
    prefix.len() == expected.len()
        && prefix.iter().zip(expected).all(|(actual, expected)| {
            *actual <= 0x7f
                && *expected <= 0x7f
                && (*actual as u8).eq_ignore_ascii_case(&(*expected as u8))
        })
}

#[cfg(target_os = "windows")]
fn open_resolved_route_leases(
    path: &Path,
    retained_handles: &mut usize,
) -> Option<Vec<std::fs::File>> {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    let mut prefixes = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        prefixes.push(prefix.clone());
    }

    let mut pins = Vec::new();
    for prefix in prefixes {
        let no_follow = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&prefix)
            .ok()?;
        let is_reparse =
            no_follow.metadata().ok()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
        let resolved = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(&prefix)
            .ok()?;
        if !resolved.metadata().ok()?.is_dir() {
            return None;
        }
        if is_reparse {
            retain_route_pin(&mut pins, no_follow, retained_handles)?;
        }
        retain_route_pin(&mut pins, resolved, retained_handles)?;
    }
    Some(pins)
}

#[cfg(target_os = "windows")]
fn final_path_from_handle(file: &std::fs::File) -> Option<PathBuf> {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW,
        VOLUME_NAME_DOS,
    };
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};

    let mut buffer = vec![0u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                ::windows::Win32::Foundation::HANDLE(file.as_raw_handle() as _),
                &mut buffer,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        } as usize;
        if length == 0 {
            return None;
        }
        if length >= buffer.len() {
            buffer.resize(length + 1, 0);
            continue;
        }
        buffer.truncate(length);
        return Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
    }
}

#[cfg(target_os = "windows")]
fn folder_pin_is_current(
    target: &Target,
    expected: &EntrySourceIdentity,
    pin: &std::fs::File,
    stamp: &SourceStamp,
) -> bool {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    use std::os::windows::fs::MetadataExt;

    pin.metadata().ok().is_some_and(|metadata| {
        metadata.is_dir()
            && metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 != 0
            && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            && SourceStamp::from_metadata(&metadata) == *stamp
            && filesystem_identity_from_handle(pin).as_ref()
                == Some(&expected.filesystem_identity())
            && expected.matches_path(&target.path)
            && crate::thumbnails::epoch_of(&target.path) == target.epoch
    })
}

#[cfg(target_os = "windows")]
fn open_pinned_child(
    path: &Path,
    expected: &EntrySourceIdentity,
) -> Option<(std::fs::File, SourceStamp)> {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
    };
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        // write/delete shareを許さず、decode中の内容変更とpath差替えを防ぐ。
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 != 0
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || expected.kind != crate::directory::EntryKind::File
        || !expected.matches_path(path)
        || filesystem_identity_from_handle(&file)? != expected.filesystem_identity()
    {
        return None;
    }
    Some((file, SourceStamp::from_metadata(&metadata)))
}

#[cfg(target_os = "windows")]
fn child_pin_is_current(
    path: &Path,
    expected: &EntrySourceIdentity,
    pin: &std::fs::File,
    stamp: &SourceStamp,
    epoch: MediaEpoch,
) -> bool {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    use std::os::windows::fs::MetadataExt;

    pin.metadata().ok().is_some_and(|metadata| {
        metadata.is_file()
            && metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            && SourceStamp::from_metadata(&metadata) == *stamp
            && filesystem_identity_from_handle(pin).as_ref()
                == Some(&expected.filesystem_identity())
            && expected.matches_path(path)
            && crate::thumbnails::epoch_of(path) == epoch
    })
}

#[cfg(target_os = "windows")]
fn filesystem_identity_from_handle(file: &std::fs::File) -> Option<FilesystemIdentity> {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    use std::os::windows::io::AsRawHandle;

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut information).ok()?;
    }
    Some(FilesystemIdentity::from_windows_parts(
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

fn render_image(mut rgba: image::RgbaImage) -> RenderImage {
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    RenderImage::new(vec![image::Frame::new(rgba)])
}

async fn decode_video(target: &Target) -> Option<Decoded> {
    if crate::thumbnails::epoch_of(&target.path) != target.epoch {
        return None;
    }
    let Some(expected) = target
        .source_identity
        .as_ref()
        .filter(|identity| identity.kind == crate::directory::EntryKind::File)
    else {
        return Some(Decoded::with_epoch(
            Content::Failed,
            target.path.clone(),
            target.epoch,
        ));
    };
    if !video_source_identity_is_current(target, expected)
        || crate::thumbnails::epoch_of(&target.path) != target.epoch
    {
        return None;
    }
    let _permit = crate::thumbnails::decode_slots().acquire().await;
    if crate::thumbnails::epoch_of(&target.path) != target.epoch {
        return None;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = expected;
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        let Some((pin, before)) = open_pinned_child(&target.path, expected) else {
            return video_open_failure_if_current(target, expected);
        };
        let file_size = before.length;
        if !video_pin_is_current(target, expected, &pin, &before) {
            return None;
        }

        let deadline = Instant::now() + VIDEO_WALL_BUDGET;
        let work_deadline = deadline - VIDEO_CLEANUP_RESERVE;
        let probed = probe_video_metadata(&target.path, work_deadline, deadline);
        if !video_pin_is_current(target, expected, &pin, &before) {
            return None;
        }
        let seek = probed
            .as_ref()
            .and_then(|metadata| metadata.duration_seconds)
            .map_or(0.0, |duration| duration * 0.1);
        let frame = if Instant::now() < work_deadline {
            extract_video_frame(
                &target.path,
                seek,
                target.spec.max_dimension,
                work_deadline,
                deadline,
            )
            .map(Arc::new)
        } else {
            None
        };
        if !video_pin_is_current(target, expected, &pin, &before) {
            return None;
        }
        let metadata = probed.unwrap_or(VideoMetadata {
            duration_seconds: None,
            width: None,
            height: None,
        });
        Some(Decoded::with_pins_and_epochs(
            Content::Video(VideoPreview {
                frame,
                duration_seconds: metadata.duration_seconds,
                width: metadata.width,
                height: metadata.height,
                file_size,
            }),
            vec![pin],
            vec![(target.path.clone(), target.epoch)],
        ))
    }
}

fn video_source_identity_is_current(target: &Target, expected: &EntrySourceIdentity) -> bool {
    expected.matches_path(&target.path)
}

#[cfg(target_os = "windows")]
fn video_open_failure_if_current(
    target: &Target,
    expected: &EntrySourceIdentity,
) -> Option<Decoded> {
    if !video_source_identity_is_current(target, expected)
        || crate::thumbnails::epoch_of(&target.path) != target.epoch
    {
        return None;
    }

    let current = crate::directory::EntrySourceIdentity::from_path(
        &target.path,
        crate::directory::EntryKind::File,
    )?;
    if &current != expected || crate::thumbnails::epoch_of(&target.path) != target.epoch {
        return None;
    }

    Some(Decoded::with_epoch(
        Content::Failed,
        target.path.clone(),
        target.epoch,
    ))
}

#[cfg(target_os = "windows")]
fn video_pin_is_current(
    target: &Target,
    expected: &EntrySourceIdentity,
    pin: &std::fs::File,
    stamp: &SourceStamp,
) -> bool {
    pin.metadata()
        .ok()
        .filter(|metadata| metadata.is_file())
        .is_some_and(|metadata| SourceStamp::from_metadata(&metadata) == *stamp)
        && filesystem_identity_from_handle(pin).as_ref() == Some(&expected.filesystem_identity())
        && video_source_identity_is_current(target, expected)
        && crate::thumbnails::epoch_of(&target.path) == target.epoch
}

struct VideoMetadata {
    duration_seconds: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
}

fn probe_video_metadata(
    path: &Path,
    work_deadline: Instant,
    deadline: Instant,
) -> Option<VideoMetadata> {
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration:stream=width,height",
        "-select_streams",
        "v:0",
        "-of",
        "default=noprint_wrappers=1",
    ]);
    command.arg(path);
    let output = run_bounded_child(
        command,
        VIDEO_METADATA_STDOUT_LIMIT,
        work_deadline,
        deadline,
    )?;
    let output = std::str::from_utf8(&output).ok()?;
    let mut metadata = VideoMetadata {
        duration_seconds: None,
        width: None,
        height: None,
    };
    for line in output.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key {
            "duration" => {
                metadata.duration_seconds = value
                    .parse::<f64>()
                    .ok()
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0);
            }
            "width" => metadata.width = value.parse::<u32>().ok().filter(|value| *value > 0),
            "height" => metadata.height = value.parse::<u32>().ok().filter(|value| *value > 0),
            _ => {}
        }
    }
    Some(metadata)
}

fn extract_video_frame(
    path: &Path,
    seek: f64,
    requested_dimension: u32,
    work_deadline: Instant,
    deadline: Instant,
) -> Option<RenderImage> {
    let dimension = requested_dimension.clamp(1, VIDEO_MAX_DIMENSION);
    let scale = format!(
        "scale=w='min({dimension},iw)':h='min({dimension},ih)':force_original_aspect_ratio=decrease"
    );
    let seek = format!("{seek:.3}");
    let mut command = Command::new("ffmpeg");
    command.args(["-nostdin", "-v", "error", "-ss", &seek, "-i"]);
    command.arg(path);
    command.args([
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-frames:v",
        "1",
        "-threads",
        "1",
        "-vf",
        &scale,
        "-f",
        "image2pipe",
        "-vcodec",
        "png",
        "pipe:1",
    ]);
    let bytes = run_bounded_child(command, VIDEO_STDOUT_LIMIT, work_deadline, deadline)?;
    decode_bounded_video_png(&bytes, dimension)
}

/// `output()` は stdout を制限なく集めるため、上限付き reader と監視側を分ける。
/// reader thread を分け、deadline と上限超過を監視する側から child を停止する。
fn run_bounded_child(
    mut command: Command,
    stdout_limit: usize,
    work_deadline: Instant,
    cleanup_deadline: Instant,
) -> Option<Vec<u8>> {
    use std::sync::mpsc;

    command.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn().ok()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::Builder::new()
        .name("quick-preview-video-stdout".into())
        .spawn(move || {
            let mut bytes = Vec::with_capacity(stdout_limit.min(64 * 1024));
            let result = stdout
                .take(stdout_limit as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()
                .filter(|_| bytes.len() <= stdout_limit)
                .map(|_| bytes);
            let _ = sender.send(result);
        });
    let reader = match reader {
        Ok(reader) => reader,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    let mut status = None;
    let mut output = None;
    let mut failed = false;
    loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                    failed = true;
                    break;
                }
                Ok(Some(bytes)) => output = Some(bytes),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit.success());
                break;
            }
            Ok(None) => {}
            Err(_) => {
                failed = true;
                break;
            }
        }
        if Instant::now() >= work_deadline {
            failed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    if failed || status != Some(true) {
        let _ = child.kill();
    }
    let exit = child.wait().ok();
    if output.is_none() && !failed {
        output = receiver
            .recv_timeout(cleanup_deadline.saturating_duration_since(Instant::now()))
            .ok()
            .flatten();
    }
    // この child の終了で pipe を閉じ、reader を join してから戻す。
    reader.join().ok()?;
    if failed || !exit.is_some_and(|exit| exit.success()) || Instant::now() > cleanup_deadline {
        return None;
    }
    output
}

fn decode_bounded_video_png(bytes: &[u8], max_dimension: u32) -> Option<RenderImage> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > max_dimension
        || height > max_dimension
        || u64::from(width) * u64::from(height) > VIDEO_MAX_PIXELS
    {
        return None;
    }
    let mut rgba = image::DynamicImage::from_decoder(decoder).ok()?.to_rgba8();
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Some(RenderImage::new(vec![image::Frame::new(rgba)]))
}

/// Keep static-image decoding and its small LRU independent from text content.
async fn decode_image(target: Target) -> Option<Decoded> {
    let Some(expected) = target
        .source_identity
        .as_ref()
        .filter(|id| id.kind == crate::directory::EntryKind::File)
    else {
        return Some(Decoded::without_lease(Content::Failed));
    };
    let _permit = crate::thumbnails::decode_slots().acquire().await;
    #[cfg(target_os = "windows")]
    let (mut pin, source, identity) = {
        let Some((pin, source)) = open_pinned_child(&target.path, expected) else {
            return Some(Decoded::without_lease(Content::Failed));
        };
        if !expected.matches_file(&pin) {
            return Some(Decoded::without_lease(Content::Failed));
        }
        if crate::thumbnails::epoch_of(&target.path) != target.epoch {
            return None;
        }
        (pin, source, expected.filesystem_identity())
    };
    #[cfg(not(target_os = "windows"))]
    {
        let _ = expected;
        return Some(Decoded::without_lease(Content::Failed));
    }
    let key = CacheKey {
        entry: target.key.clone(),
        path: target.path.clone(),
        spec: target.spec,
        epoch: target.epoch,
        source: source.clone(),
        identity: identity.clone(),
    };
    if let Some((_, image)) = cache()
        .lock()
        .ok()?
        .entries
        .iter()
        .find(|(candidate, _)| candidate == &key)
    {
        return Some(Decoded::with_pins_and_epochs(
            Content::Image(image.clone()),
            vec![pin],
            vec![(target.path.clone(), target.epoch)],
        ));
    }
    let rgba = match thumb_engine::generate_thumbnail_from_reader(
        &target.path,
        &mut pin,
        target.spec.max_dimension,
        thumb_engine::FitMode::Fit,
    ) {
        thumb_engine::ThumbnailResult::Generated(image) => image.to_rgba8(),
        _ => {
            return Some(Decoded::with_pins_and_epochs(
                Content::Failed,
                vec![pin],
                vec![(target.path.clone(), target.epoch)],
            ));
        }
    };
    if !expected.matches_file(&pin)
        || pin
            .metadata()
            .ok()
            .is_none_or(|metadata| SourceStamp::from_metadata(&metadata) != source)
        || expected.filesystem_identity() != identity
        || crate::thumbnails::epoch_of(&target.path) != target.epoch
    {
        return None;
    }
    let mut rgba = rgba;
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let image = Arc::new(RenderImage::new(vec![image::Frame::new(rgba)]));
    let mut state = cache().lock().ok()?;
    state.entries.retain(|(candidate, _)| candidate != &key);
    state.entries.push_front((key, image.clone()));
    while state.entries.len() > 8 {
        state.entries.pop_back();
    }
    Some(Decoded::with_pins_and_epochs(
        Content::Image(image),
        vec![pin],
        vec![(target.path.clone(), target.epoch)],
    ))
}

pub(crate) fn is_text_preview_target(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "txt"
                    | "md"
                    | "rs"
                    | "toml"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "log"
                    | "csv"
                    | "tsv"
                    | "ini"
                    | "css"
                    | "html"
                    | "xml"
                    | "js"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "py"
                    | "bat"
                    | "ps1"
            )
        })
}

fn read_text_preview(target: &Target) -> Result<TextPreview, ()> {
    let expected = target
        .source_identity
        .as_ref()
        .filter(|identity| identity.kind == crate::directory::EntryKind::File)
        .map(EntrySourceIdentity::filesystem_identity)
        .ok_or(())?;
    let mut file = open_text_file(&target.path, &expected)?;
    let before = file.metadata().map_err(|_| ())?;
    let before_stamp = SourceStamp::from_metadata(&before);
    let mut bytes = Vec::with_capacity(TEXT_READ_LIMIT + 1);
    file.by_ref()
        .take((TEXT_READ_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    let after = file.metadata().map_err(|_| ())?;
    if SourceStamp::from_metadata(&after) != before_stamp {
        return Err(());
    }
    let truncated = bytes.len() > TEXT_READ_LIMIT;
    bytes.truncate(TEXT_READ_LIMIT);
    Ok(decode_text_preview(bytes, truncated))
}

#[cfg(target_os = "windows")]
fn open_text_file(
    path: &std::path::Path,
    expected: &FilesystemIdentity,
) -> Result<std::fs::File, ()> {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY.0 != 0
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err(());
    }
    let mut information =
        ::windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut information)
            .map_err(|_| ())?;
    }
    let actual = FilesystemIdentity::from_windows_parts(
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    );
    (actual == *expected).then_some(file).ok_or(())
}

#[cfg(not(target_os = "windows"))]
fn open_text_file(
    _path: &std::path::Path,
    _expected: &FilesystemIdentity,
) -> Result<std::fs::File, ()> {
    Err(())
}

fn decode_text_preview(bytes: Vec<u8>, mut truncated: bool) -> TextPreview {
    let (mut text, encoding, lossy) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let (text, lossy) = decode_utf8(rest);
        (text, "UTF-8 BOM", lossy)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        decode_utf16_preview(rest, true, "UTF-16 LE")
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        decode_utf16_preview(rest, false, "UTF-16 BE")
    } else if let Ok(text) = std::str::from_utf8(&bytes) {
        (text.to_owned(), "UTF-8", false)
    } else {
        let (text, _, lossy) = encoding_rs::SHIFT_JIS.decode(&bytes);
        (text.into_owned(), "Shift_JIS fallback", lossy)
    };
    text = text.replace('\0', " ");
    if text.len() > TEXT_OUTPUT_LIMIT {
        let mut boundary = TEXT_OUTPUT_LIMIT;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        truncated = true;
    }
    TextPreview {
        text,
        encoding,
        lossy,
        truncated,
    }
}

fn decode_utf8(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

fn decode_utf16_preview(
    bytes: &[u8],
    little_endian: bool,
    encoding: &'static str,
) -> (String, &'static str, bool) {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        units.push(if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        });
    }
    let odd_tail = !chunks.remainder().is_empty();
    let strict = String::from_utf16(&units);
    let lossy = odd_tail || strict.is_err();
    let text = strict.unwrap_or_else(|_| String::from_utf16_lossy(&units));
    (text, encoding, lossy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{directory::EntryKind, entry_identity::EntryKey};

    fn target(path: &str, location_generation: u64) -> Target {
        let path = PathBuf::from(path);
        Target {
            tab_id: 1,
            location_generation,
            key: EntryKey::path(path.clone(), EntryKind::File),
            path,
            source_identity: None,
            fallback_icon: None,
            epoch: MediaEpoch::default(),
            spec: DecodeSpec::default(),
        }
    }

    #[test]
    fn duplicate_target_is_deduplicated() {
        let mut session = Session::default();
        assert!(session.open_or_update(target("a.png", 1)));
        assert!(!session.open_or_update(target("a.png", 1)));
    }

    #[test]
    fn fallback_icon_is_part_of_target_deduplication() {
        let mut session = Session::default();
        let mut first = target("a.unknown", 1);
        first.fallback_icon = Some(IconId::File);
        assert!(session.open_or_update(first.clone()));
        assert!(!session.open_or_update(first));

        let mut changed_icon = target("a.unknown", 1);
        changed_icon.fallback_icon = Some(IconId::FileText);
        assert!(session.open_or_update(changed_icon));
    }

    #[test]
    fn stale_completions_cannot_reopen_or_repopulate() {
        let mut session = Session::default();
        let a = target("a.png", 1);
        session.open_or_update(a.clone());
        let old_a = Completion {
            target: a,
            target_generation: session.target_generation(),
        };
        session.open_or_update(target("b.png", 1));
        assert!(!session.accepts(&old_a, 1, 1, MediaEpoch::default()));
        session.close();
        assert!(!session.accepts(&old_a, 1, 1, MediaEpoch::default()));
    }

    #[test]
    fn retargeted_preview_keeps_late_a_result_out_and_commits_b() {
        let mut session = Session::default();
        let a = target("a.png", 1);
        assert!(session.open_or_update(a.clone()));
        let late_a = Completion {
            target: a,
            target_generation: session.target_generation(),
        };

        let b = target("b.png", 1);
        assert!(session.open_or_update(b.clone()));
        let b_completion = Completion {
            target: b.clone(),
            target_generation: session.target_generation(),
        };

        let a_result = Content::Text(TextPreview {
            text: "A result".into(),
            encoding: "UTF-8",
            lossy: false,
            truncated: false,
        });
        assert!(!session.commit(late_a, a_result, 1, 1, b.epoch,));
        assert!(matches!(session.content(), Some(Content::Loading)));

        let b_result = Content::Text(TextPreview {
            text: "B result".into(),
            encoding: "UTF-8",
            lossy: false,
            truncated: false,
        });
        assert!(session.commit(b_completion, b_result, 1, 1, b.epoch,));
        match session.content() {
            Some(Content::Text(preview)) => assert_eq!(preview.text, "B result"),
            _ => panic!("B preview result should be committed"),
        }
    }

    #[test]
    fn location_reuse_and_epoch_are_identity_gates() {
        let mut session = Session::default();
        let mut a = target("a.png", 1);
        session.open_or_update(a.clone());
        let completion = Completion {
            target: a.clone(),
            target_generation: session.target_generation(),
        };
        a.location_generation = 2;
        assert!(!session.accepts(
            &Completion {
                target: a,
                ..completion.clone()
            },
            1,
            2,
            completion.target.epoch,
        ));
        assert!(session.accepts(&completion, 1, 1, completion.target.epoch));
        assert_ne!(completion.target, target("a.png", 2));
    }

    #[test]
    fn current_epoch_rejects_a_late_completion() {
        let path = std::env::temp_dir().join(format!(
            "gpui-filer-quick-preview-epoch-{}",
            std::process::id()
        ));
        let mut session = Session::default();
        let target = target(path.to_str().unwrap(), 1);
        session.open_or_update(target.clone());
        let completion = Completion {
            target: target.clone(),
            target_generation: session.target_generation(),
        };
        crate::thumbnails::invalidate(std::slice::from_ref(&path));
        assert!(!session.accepts(&completion, 1, 1, crate::thumbnails::epoch_of(&path),));
    }

    #[test]
    fn current_folder_open_failure_is_failed_but_epoch_staleness_is_discarded() {
        let path =
            std::env::temp_dir().join(format!("gpui-filer-missing-folder-{}", std::process::id()));
        let mut folder = target(path.to_str().unwrap(), 1);
        folder.key = EntryKey::path(path.clone(), EntryKind::Folder);
        folder.epoch = crate::thumbnails::epoch_of(&path);
        assert!(matches!(
            folder_open_failure_if_current(&folder).map(|decoded| decoded.content),
            Some(Content::Failed)
        ));

        crate::thumbnails::invalidate(std::slice::from_ref(&path));
        assert!(folder_open_failure_if_current(&folder).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn failed_windows_folder_open_returns_terminal_failure_for_current_epoch() {
        let path = std::env::temp_dir().join(format!(
            "gpui-filer-preview-folder-open-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let identity = EntrySourceIdentity::from_path(&path, EntryKind::Folder).unwrap();
        std::fs::remove_dir(&path).unwrap();
        let mut folder = target(path.to_str().unwrap(), 1);
        folder.key = EntryKey::path(path.clone(), EntryKind::Folder);
        folder.source_identity = Some(identity.clone());
        folder.epoch = crate::thumbnails::epoch_of(&path);

        assert!(open_pinned_folder(&folder, &identity).is_none());
        assert!(matches!(
            folder_open_failure_if_current(&folder).map(|decoded| decoded.content),
            Some(Content::Failed)
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_folder_route_lease_blocks_empty_ancestor_replacement() {
        let root = std::env::temp_dir().join(format!(
            "gpui-filer-preview-route-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ancestor = root.join("accepted-route");
        let folder = ancestor.join("empty");
        let replacement = root.join("replacement-route");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(replacement.join("empty")).unwrap();

        let route_pins = open_folder_route_leases(&folder).unwrap();
        assert!(!route_pins.is_empty());
        let mut request = target(folder.to_str().unwrap(), 1);
        request.key = EntryKey::path(folder.clone(), EntryKind::Folder);
        let mut session = Session::default();
        session.open_or_update(request.clone());
        let stale = Completion {
            target: request.clone(),
            target_generation: session.target_generation(),
        };
        session.open_or_update(target("retargeted", 1));
        let decoded = Decoded::with_pins_and_epochs(
            Content::FolderPreview(Arc::new(Vec::new())),
            route_pins,
            vec![(folder.clone(), request.epoch)],
        );
        // Empty folder でも route handle が残る間は ancestor を置換できない。
        assert!(std::fs::rename(&ancestor, &root.join("moved-route")).is_err());
        assert!(std::fs::read_dir(&folder).unwrap().next().is_none());

        assert!(!session.commit_decoded(stale, decoded, 1, 1, request.epoch));
        std::fs::rename(&ancestor, &root.join("moved-route")).unwrap();
        assert!(replacement.join("empty").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_folder_route_lease_pins_stable_junction_against_reparse_write() {
        use ::windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        use std::os::windows::fs::OpenOptionsExt;

        // Mounted temp volumes may not support junctions; keep this fixture on the local workspace volume.
        let temp_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/quick-preview-fixtures");
        std::fs::create_dir_all(&temp_root).unwrap();
        let root = temp_root.join(format!(
            "gpui-filer-preview-junction-route-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target_parent = root.join("other").join("parent");
        let target = target_parent.join("target");
        let moved_parent = root.join("other").join("moved-parent");
        let junction = root.join("stable-junction");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(target.join("empty")).unwrap();
        crate::operations::privileged::create_junction_direct(&target, &junction).unwrap();

        let route_pins = open_folder_route_leases(&junction.join("empty")).unwrap();
        assert!(
            !route_pins.is_empty(),
            "stable junction route must be accepted"
        );
        let write_attempt = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&junction);
        assert!(
            write_attempt.is_err(),
            "junction reparse data must not be writable while the route is leased"
        );
        assert!(
            std::fs::rename(&target_parent, &moved_parent).is_err(),
            "the junction target's separate ancestor must not be replaceable while leased"
        );

        drop(route_pins);
        std::fs::rename(&target_parent, &moved_parent).unwrap();
        std::fs::remove_dir(&junction).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_folder_route_lease_pins_nested_junction_chain_and_ancestors() {
        use ::windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        use std::os::windows::fs::OpenOptionsExt;

        let temp_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/quick-preview-fixtures");
        std::fs::create_dir_all(&temp_root).unwrap();
        let root = temp_root.join(format!(
            "gpui-filer-preview-nested-junction-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let visible = root.join("Visible");
        let other = root.join("Other_日本語");
        let replaceable = root.join("Replaceable");
        let target_parent = replaceable.join("Parent");
        let target = target_parent.join("A");
        std::fs::create_dir_all(&visible).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(target.join("empty")).unwrap();
        let first_link = visible.join("J");
        let nested_link = other.join("Q");
        crate::operations::privileged::create_junction_direct(&target, &nested_link).unwrap();
        crate::operations::privileged::create_junction_direct(&nested_link, &first_link).unwrap();

        let route_pins = open_folder_route_leases(&first_link.join("empty")).unwrap();
        assert!(!route_pins.is_empty());
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .share_mode(FILE_SHARE_READ.0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(&nested_link)
                .is_err(),
            "the hidden nested junction must remain protected from reparse mutation"
        );
        assert!(
            std::fs::rename(&other, &root.join("Other-moved")).is_err(),
            "the first junction target route must remain leased"
        );
        assert!(
            std::fs::rename(&target_parent, &replaceable.join("Parent-moved")).is_err(),
            "the nested junction target ancestor must remain leased"
        );

        drop(route_pins);
        let moved_other = root.join("Other-moved");
        std::fs::rename(&other, &moved_other).unwrap();
        std::fs::rename(&target_parent, &replaceable.join("Parent-moved")).unwrap();
        std::fs::remove_dir(&first_link).unwrap();
        std::fs::remove_dir(moved_other.join("Q")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn decoded_source_pin_survives_owner_acceptance_then_releases() {
        use ::windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        use std::os::windows::fs::OpenOptionsExt;

        let path = std::env::temp_dir().join(format!(
            "gpui-filer-preview-lease-{}.tmp",
            std::process::id()
        ));
        let replacement_path = path.with_extension("replacement");
        let _ = std::fs::remove_file(&replacement_path);
        std::fs::write(&path, b"pin lifetime").unwrap();
        let pin = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&path)
            .unwrap();

        let mut session = Session::default();
        let request = target(path.to_str().unwrap(), 1);
        session.open_or_update(request.clone());
        let completion = Completion {
            target: request.clone(),
            target_generation: session.target_generation(),
        };
        let decoded = Decoded::with_pins_and_epochs(
            Content::Failed,
            vec![pin],
            vec![(path.clone(), request.epoch)],
        );
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::rename(&path, &replacement_path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert!(session.commit_decoded(completion, decoded, 1, 1, request.epoch));
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
        std::fs::rename(&path, &replacement_path).unwrap();
        assert!(replacement_path.exists());
        std::fs::remove_file(&replacement_path).unwrap();
    }
}
