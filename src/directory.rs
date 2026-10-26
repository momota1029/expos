use gpui::{App, Asset, RenderImage};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Folder,
    File,
}

/// パスが指す先ではなく、列挙で直接観測した link / reparse point の種別。
///
/// Rust の Windows `FileType::is_symlink` は junction も含めて返す。そのためここでは
/// 未検証の reparse tag を名称へ決め打ちせず、両方を同じ安全境界に置く。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// symbolic link または junction。target を辿る mutation は未対応。
    LinkOrJunction,
    /// symbolic link / junction 以外の Windows reparse point。
    OtherReparsePoint,
}

fn link_kind(metadata: &std::fs::Metadata) -> Option<LinkKind> {
    if metadata.file_type().is_symlink() {
        return Some(LinkKind::LinkOrJunction);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;

        (metadata.file_attributes() & 0x400 != 0).then_some(LinkKind::OtherReparsePoint)
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// 列挙時に得た filesystem object identity。Everything query 前後で同じ object かを
/// best-effort で再検証するために保持する。表示 key の path とは用途が違う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntrySourceIdentity {
    pub kind: EntryKind,
    identity: crate::filesystem_identity::FilesystemIdentity,
}

impl EntrySourceIdentity {
    /// OS が公開する object identity を読む。取得できない entry は、index size なしで表示を
    /// 続ける。path text だけで外部 index 結果を再利用するより安全だからである。
    pub fn from_path(path: &Path, kind: EntryKind) -> Option<Self> {
        #[cfg(target_os = "windows")]
        let identity = windows_source_identity(path, false)?;
        #[cfg(not(target_os = "windows"))]
        let identity = {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            crate::filesystem_identity::FilesystemIdentity::from_fallback_token(format!(
                "{:?}:{}",
                metadata.modified().ok(),
                metadata.len()
            ))
        };
        Some(Self { kind, identity })
    }

    /// Capture the directory entry itself, including a reparse point rather than its target.
    pub fn from_path_no_follow(path: &Path, kind: EntryKind) -> Option<Self> {
        #[cfg(target_os = "windows")]
        let identity = windows_source_identity(path, true)?;
        #[cfg(not(target_os = "windows"))]
        let identity = {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            crate::filesystem_identity::FilesystemIdentity::from_fallback_token(format!(
                "{:?}:{}",
                metadata.modified().ok(),
                metadata.len()
            ))
        };
        Some(Self { kind, identity })
    }

    pub(crate) fn filesystem_identity(&self) -> crate::filesystem_identity::FilesystemIdentity {
        self.identity.clone()
    }

    /// Bounded same-executable helper protocol; fallback identities fail closed.
    pub(crate) fn helper_transport_parts(&self) -> Option<(u32, u64)> {
        (self.kind == EntryKind::File)
            .then(|| self.identity.helper_transport_parts())
            .flatten()
    }

    pub(crate) fn from_helper_transport_parts(kind: EntryKind, volume: u32, index: u64) -> Self {
        Self {
            kind,
            identity: crate::filesystem_identity::FilesystemIdentity::from_helper_transport_parts(
                volume, index,
            ),
        }
    }

    /// Compare an already opened regular-file handle without resolving its path again.
    pub(crate) fn matches_file(&self, file: &std::fs::File) -> bool {
        #[cfg(windows)]
        {
            self.kind == EntryKind::File
                && filesystem_identity_from_file(file).as_ref() == Some(&self.identity)
        }
        #[cfg(not(windows))]
        {
            let _ = file;
            false
        }
    }

    pub fn matches_path(&self, path: &Path) -> bool {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        let kind = if metadata.is_dir() {
            EntryKind::Folder
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            return false;
        };
        link_kind(&metadata).is_none()
            && Self::from_path(path, kind).is_some_and(|actual| actual == *self)
    }

    #[cfg(test)]
    pub fn test_folder() -> Self {
        Self::test(EntryKind::Folder, "test-folder")
    }

    #[cfg(test)]
    pub(crate) fn test(kind: EntryKind, token: impl Into<String>) -> Self {
        Self {
            kind,
            identity: crate::filesystem_identity::FilesystemIdentity::from_fallback_token(
                token.into(),
            ),
        }
    }
}

/// Everything index が返す folder size。
///
/// 通常fileの `size` と別の index fact にして、operation の total には流用しない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderSizeFact {
    pub bytes: u64,
    pub provenance: FolderSizeProvenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderSizeProvenance {
    EverythingIndex,
}

/// 一覧に出す1項目。
///
/// size / modified は表示文字列ではなく raw fact として持つ。整形は locale を
/// 知っている application 側（`shell::i18n`）の役割で、ここは filesystem が
/// 返した値をそのまま運ぶだけにする。
#[derive(Clone)]
pub struct Entry {
    /// 並べ替えや再列挙をまたいでこの項目を指すキー。
    pub key: crate::entry_identity::EntryKey,
    pub name: String,
    pub path: Option<PathBuf>,
    pub kind: EntryKind,
    /// `symlink_metadata` で直接観測した reparse point。`kind` は activation 用の
    /// link target の分類なので、これと混ぜない。
    pub link: Option<LinkKind>,
    /// LinkOrJunction の raw target。存在確認や正規化はしない。
    pub link_target: Option<PathBuf>,
    pub source_identity: Option<EntrySourceIdentity>,
    /// Windows Hidden attribute read from the entry itself with `symlink_metadata`.
    pub is_hidden: bool,
    /// filesystem が直接観測した file の確定 byte 数。folder は常に `None`。
    pub size: Option<u64>,
    /// Everything index が返した folder byte 数。`size` とは混ぜない。
    pub folder_size: Option<FolderSizeFact>,
    /// 更新日時。取得できなかった項目は `None`。
    pub modified: Option<SystemTime>,
}

impl Entry {
    pub fn is_link(&self) -> bool {
        self.link.is_some()
    }
}

/// 列挙の結果。表示文字列ではなく raw fact の集合。
pub type Listing = Arc<Vec<Entry>>;

/// 列挙した行と、同じ列挙元ディレクトリから得た identity。
#[derive(Clone)]
pub struct EnumeratedListing {
    pub entries: Listing,
    pub directory_identity: Option<crate::filesystem_identity::FilesystemIdentity>,
}

impl From<Listing> for EnumeratedListing {
    fn from(entries: Listing) -> Self {
        Self {
            entries,
            directory_identity: None,
        }
    }
}

/// 列挙の失敗。OS のメッセージはそのまま運び、翻訳は application 側で行う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryError(pub String);

impl std::fmt::Display for DirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// ディレクトリを列挙する。
///
/// 以前は GPUI の `Asset` として path をキーにしていたが、それだと同じ path の
/// 再読込が新しい読取にならない。ここは detached な関数にして、呼び出し側が
/// background executor へ渡す。freshness の判定は session が持つ。
pub fn enumerate(path: PathBuf) -> Result<EnumeratedListing, DirectoryError> {
    let (entries, directory_identity) =
        read_directory_with_identity(&path).map_err(|error| DirectoryError(error.to_string()))?;
    Ok(EnumeratedListing {
        entries: Arc::new(entries),
        directory_identity,
    })
}

#[cfg(test)]
fn read_directory(path: &Path) -> std::io::Result<Vec<Entry>> {
    read_directory_with_identity(path).map(|(entries, _)| entries)
}

fn read_directory_with_identity(
    path: &Path,
) -> std::io::Result<(
    Vec<Entry>,
    Option<crate::filesystem_identity::FilesystemIdentity>,
)> {
    #[cfg(windows)]
    let (route, directory_identity) = windows_directory_route(path);
    #[cfg(not(windows))]
    let directory_identity = None;

    let mut entries = Vec::new();
    for item in std::fs::read_dir(path)? {
        let item = item?;
        // copy は完走まで requested name を publish せず、current process 固有の
        // owned staging file/directory を置く。watch/read がその瞬間を拾っても、内部名を
        // row として見せない。PID を含まない名前は外部や障害後の可能性があるので隠さない。
        if is_live_copy_staging_name(&item.file_name()) {
            continue;
        }
        let item_path = item.path();
        // `metadata` は link target を追う。表示と操作の安全境界には、必ず
        // `symlink_metadata` が返す link object 自身の事実も残す。
        let link_metadata = std::fs::symlink_metadata(&item_path)?;
        let is_hidden = metadata_is_hidden(&link_metadata);
        let link = link_kind(&link_metadata);
        let link_target = matches!(link, Some(LinkKind::LinkOrJunction))
            .then(|| std::fs::read_link(&item_path).ok())
            .flatten();
        // target が既に消えた broken link も、一覧全体を unavailable にせず link として
        // 残す。開く処理は通常どおり OS に任せる。
        // `DirEntry::metadata` は Windows の junction を reparse object のまま返すことが
        // ある。activation の分類だけは明示的な `fs::metadata` で target を見る。
        let target_metadata = std::fs::metadata(&item_path).ok();
        let is_folder = target_metadata
            .as_ref()
            .is_some_and(std::fs::Metadata::is_dir)
            || (target_metadata.is_none() && link_metadata.is_dir());
        let kind = if is_folder {
            EntryKind::Folder
        } else {
            EntryKind::File
        };
        // EntrySourceIdentity は no-follow handle から link object 自身を記録する。
        // これは観測事実であり、mutation は各 operation の no-follow adapter が別途検証する。
        let source_identity = EntrySourceIdentity::from_path_no_follow(&item_path, kind);
        entries.push(Entry {
            key: crate::entry_identity::EntryKey::path(item_path.clone(), kind),
            name: item.file_name().to_string_lossy().into_owned(),
            path: Some(item_path),
            kind,
            link,
            link_target,
            source_identity,
            is_hidden,
            // フォルダの合計サイズは列挙時に数えない。Everything 連携で埋める。
            size: (!is_folder && link.is_none())
                .then(|| target_metadata.as_ref().map(std::fs::Metadata::len))
                .flatten(),
            folder_size: None,
            modified: link_metadata.modified().ok(),
        });
    }
    entries.sort_by_cached_key(|entry| {
        (
            !matches!(entry.kind, EntryKind::Folder),
            entry.name.to_lowercase(),
        )
    });
    #[cfg(windows)]
    drop(route);
    Ok((entries, directory_identity))
}

#[cfg(windows)]
fn metadata_is_hidden(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes() & 0x2 != 0 // FILE_ATTRIBUTE_HIDDEN
}

#[cfg(not(windows))]
fn metadata_is_hidden(_: &std::fs::Metadata) -> bool {
    false
}

/// path-based の `read_dir` が同じ object を参照するように、祖先から順に no-follow
/// handle を保持する。DELETE share を外し、途中の rename / junction 置換を抑止する。
#[cfg(windows)]
fn windows_directory_route(
    path: &Path,
) -> (
    Option<Vec<std::fs::File>>,
    Option<crate::filesystem_identity::FilesystemIdentity>,
) {
    use ::windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use std::os::windows::fs::OpenOptionsExt;

    let mut ancestors: Vec<_> = path.ancestors().collect();
    ancestors.reverse();
    let mut handles = Vec::with_capacity(ancestors.len());
    for ancestor in ancestors {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
        let Ok(handle) = options.open(ancestor) else {
            return (None, None);
        };
        let metadata = match handle.metadata() {
            Ok(metadata) => metadata,
            Err(_) => return (None, None),
        };
        use std::os::windows::fs::MetadataExt;
        let attributes = metadata.file_attributes();
        if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return (None, None);
        }
        handles.push(handle);
    }
    let identity = handles.last().and_then(filesystem_identity_from_file);
    // Keep the route lock alive even when identity probing fails, so enumeration still has
    // stable path binding; caller receives None and guarded consumers fail closed.
    (Some(handles), identity)
}

/// `MetadataExt` の Windows object identity は stable ではないため、opened handle から
/// volume serial + file index を得る。handle は probe 中だけ保持し、listing の lifetime
/// まで lock しない。
#[cfg(target_os = "windows")]
fn windows_source_identity(
    path: &Path,
    no_follow: bool,
) -> Option<crate::filesystem_identity::FilesystemIdentity> {
    use ::windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE},
            Storage::FileSystem::{
                CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            },
        },
        core::PCWSTR,
    };
    use std::os::windows::ffi::OsStrExt;

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = if no_follow {
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
    } else {
        FILE_FLAG_BACKUP_SEMANTICS
    };
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
        .ok()?
    };
    let handle = Handle(handle);
    filesystem_identity_from_raw_handle(handle.0)
}

#[cfg(windows)]
fn filesystem_identity_from_file(
    file: &std::fs::File,
) -> Option<crate::filesystem_identity::FilesystemIdentity> {
    use ::windows::Win32::Foundation::HANDLE;
    use std::os::windows::io::AsRawHandle;
    filesystem_identity_from_raw_handle(HANDLE(file.as_raw_handle() as _))
}

#[cfg(windows)]
fn filesystem_identity_from_raw_handle(
    handle: ::windows::Win32::Foundation::HANDLE,
) -> Option<crate::filesystem_identity::FilesystemIdentity> {
    use ::windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info).ok()? };
    Some(
        crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ),
    )
}

fn is_live_copy_staging_name(name: &std::ffi::OsStr) -> bool {
    let prefix = format!(".gpui-filer-copy-{}-", std::process::id());
    name.to_string_lossy().starts_with(&prefix)
}

pub type PreviewImages = Arc<Vec<Arc<RenderImage>>>;

#[derive(Clone)]
pub struct PreviewOutput {
    pub images: PreviewImages,
    pub reason: Option<crate::thumbnails::PreviewFailureReason>,
}

pub struct Preview;
impl Asset for Preview {
    type Source = crate::thumbnails::Key;
    type Output = PreviewOutput;
    fn load(
        (path, size, folder, epoch): Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let background = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            if folder {
                read_previews(path, size, epoch, background).await
            } else {
                let _slot = crate::thumbnails::decode_slots().acquire().await;
                match crate::thumbnails::load_with_reason(&path, size) {
                    Ok(image) => PreviewOutput {
                        images: Arc::new(vec![Arc::new(image)]),
                        reason: None,
                    },
                    Err(reason) => PreviewOutput {
                        images: Arc::new(Vec::new()),
                        reason: Some(reason),
                    },
                }
            }
        })
    }
}

const FOLDER_PREVIEW_SCAN_BATCH: usize = 32;
const FOLDER_PREVIEW_SCAN_YIELD: std::time::Duration = std::time::Duration::from_millis(1);

async fn read_previews(
    path: PathBuf,
    size: u32,
    epoch: crate::thumbnails::MediaEpoch,
    background: gpui::BackgroundExecutor,
) -> PreviewOutput {
    let mut images = Vec::new();
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(_) => {
            return PreviewOutput {
                images: Arc::new(images),
                reason: Some(crate::thumbnails::PreviewFailureReason::FolderUnavailable),
            };
        }
    };
    let mut entries = entries;
    let mut finished = false;
    loop {
        if crate::thumbnails::epoch_of(&path) != epoch {
            break;
        }
        let slot = crate::thumbnails::decode_slots().acquire().await;
        if crate::thumbnails::epoch_of(&path) != epoch {
            drop(slot);
            break;
        }
        for _ in 0..FOLDER_PREVIEW_SCAN_BATCH {
            let Some(entry) = entries.next() else {
                finished = true;
                break;
            };
            let Ok(entry) = entry else {
                continue;
            };
            let child_path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            // 動画は ffmpeg の起動を伴うため、フォルダーの見本には使わない。
            // clip/mdp/psd/kra は v5 では対象外だったが、絵を扱うフォルダーで
            // 見本が空になるのを避けるため v6 では含める。
            if crate::thumbnails::is_video(&child_path) {
                continue;
            }
            let image = crate::thumbnails::load_with_reason(&child_path, size);
            if crate::thumbnails::epoch_of(&path) != epoch {
                finished = true;
                break;
            }
            if let Ok(image) = image {
                images.push(Arc::new(image));
                if images.len() == 3 {
                    finished = true;
                    break;
                }
            }
        }
        drop(slot);
        if finished {
            break;
        }
        // Huge folders must give other preview tasks a chance between bounded batches.
        background.timer(FOLDER_PREVIEW_SCAN_YIELD).await;
    }
    let reason = images
        .is_empty()
        .then_some(crate::thumbnails::PreviewFailureReason::FolderNoPreview);
    PreviewOutput {
        images: Arc::new(images),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    async fn absent_directory_is_reported(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_directory(&directory.path().join("missing")).is_err());
        let path = directory.path().join("missing");
        let background = cx.background_executor.clone();
        let scan_executor = background.clone();
        assert!(
            background
                .spawn(read_previews(
                    path.clone(),
                    80,
                    crate::thumbnails::epoch_of(&path),
                    scan_executor,
                ))
                .await
                .images
                .is_empty()
        );
    }

    #[test]
    fn hides_only_this_process_live_copy_staging_entries() {
        let directory = tempfile::tempdir().unwrap();
        let owned = directory
            .path()
            .join(format!(".gpui-filer-copy-{}-1", std::process::id()));
        std::fs::write(&owned, b"staging").unwrap();
        let owned_directory = directory
            .path()
            .join(format!(".gpui-filer-copy-{}-2", std::process::id()));
        std::fs::create_dir(&owned_directory).unwrap();
        std::fs::write(
            directory.path().join(".gpui-filer-copy-external-1"),
            b"keep",
        )
        .unwrap();
        std::fs::write(directory.path().join("visible.txt"), b"keep").unwrap();

        let names: Vec<_> = read_directory(directory.path())
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert!(
            !names
                .iter()
                .any(|name| name == &owned.file_name().unwrap().to_string_lossy())
        );
        assert!(
            !names
                .iter()
                .any(|name| name == &owned_directory.file_name().unwrap().to_string_lossy())
        );
        assert!(
            names
                .iter()
                .any(|name| name == ".gpui-filer-copy-external-1")
        );
        assert!(names.iter().any(|name| name == "visible.txt"));
    }

    #[gpui::test]
    async fn preview_skips_bad_images_limits_count_and_crops_square(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("broken.png"), b"not an image").unwrap();
        std::fs::create_dir(directory.path().join("folder.png")).unwrap();
        for n in 0..5 {
            let image = image::RgbaImage::from_pixel(200, 100, image::Rgba([255, 0, 0, 255]));
            image
                .save(directory.path().join(format!("{n}.png")))
                .unwrap();
        }
        let path = directory.path().to_path_buf();
        let background = cx.background_executor.clone();
        let scan_executor = background.clone();
        let previews = background
            .spawn(read_previews(
                path.clone(),
                80,
                crate::thumbnails::epoch_of(&path),
                scan_executor,
            ))
            .await;
        assert_eq!(previews.images.len(), 3);
        for preview in previews.images.iter() {
            // v5 のサムネイルと同じ Cover なので、元が横長でも正方形に切り出される。
            assert_eq!(
                preview.size(0),
                gpui::size(gpui::DevicePixels(80), gpui::DevicePixels(80))
            );
            assert_eq!(&preview.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        }
        let entries = read_directory(directory.path()).unwrap();
        assert!(matches!(entries[0].kind, EntryKind::Folder));
        assert_eq!(entries.len(), 7);
    }

    #[test]
    fn entries_carry_raw_size_and_modified_facts() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), b"hello").unwrap();
        std::fs::create_dir(directory.path().join("folder")).unwrap();

        let entries = read_directory(directory.path()).unwrap();
        let folder = entries
            .iter()
            .find(|entry| matches!(entry.kind, EntryKind::Folder))
            .unwrap();
        let file = entries
            .iter()
            .find(|entry| matches!(entry.kind, EntryKind::File))
            .unwrap();

        // 表示文字列ではなく byte 数で持つ。
        assert_eq!(file.size, Some(5));
        assert_eq!(file.link, None);
        assert_eq!(file.folder_size, None);
        // フォルダの合計サイズは Everything 連携で埋めるまで未取得。
        assert_eq!(folder.size, None);
        assert_eq!(folder.folder_size, None);
        // 更新日時は timestamp のまま運び、整形は application 側で行う。
        assert!(file.modified.is_some());
        assert!(folder.modified.is_some());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn junction_keeps_its_link_identity_and_is_not_a_folder_size_source() {
        // TEMPがjunction非対応volumeでもfixtureを作れるよう、crateと同じvolumeへ置く。
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let target = directory.path().join("target");
        let junction = directory.path().join("junction");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside.txt"), b"target contents").unwrap();

        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J must create the test junction");

        let entry = read_directory(directory.path())
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "junction")
            .unwrap();
        assert_eq!(entry.kind, EntryKind::Folder);
        assert_eq!(entry.link, Some(LinkKind::LinkOrJunction));
        assert!(entry.is_link());
        assert!(entry.source_identity.is_some());
        assert_eq!(entry.size, None);
        assert_eq!(entry.folder_size, None);
    }

    #[cfg(windows)]
    #[test]
    fn enumeration_reads_hidden_attribute_from_no_follow_metadata() {
        use std::os::windows::ffi::OsStrExt as _;
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

        let directory = tempfile::tempdir().unwrap();
        let hidden_path = directory.path().join("hidden.txt");
        std::fs::write(&hidden_path, b"hidden").unwrap();
        let wide = hidden_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe { SetFileAttributesW(windows::core::PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
            .unwrap();

        let entry = read_directory(directory.path())
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "hidden.txt")
            .unwrap();
        assert!(entry.is_hidden);
    }

    #[cfg(windows)]
    #[test]
    fn hidden_link_classification_uses_the_link_object_not_its_target() {
        use std::os::windows::{ffi::OsStrExt as _, fs::symlink_file};
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("hidden-target.txt");
        let link = directory.path().join("visible-link.txt");
        std::fs::write(&target, b"hidden").unwrap();
        let wide = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe { SetFileAttributesW(windows::core::PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
            .unwrap();
        if symlink_file(&target, &link).is_err() {
            return;
        }

        let link_metadata = std::fs::symlink_metadata(&link).unwrap();
        let target_metadata = std::fs::metadata(&link).unwrap();
        let link_hidden = metadata_is_hidden(&link_metadata);
        let target_hidden = metadata_is_hidden(&target_metadata);
        let entry = read_directory(directory.path())
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "visible-link.txt")
            .unwrap();
        assert_eq!(target_hidden, true);
        assert_eq!(entry.is_hidden, link_hidden);
    }
}
