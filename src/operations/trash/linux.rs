use std::{
    collections::HashSet,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use rustix::fs::{CWD, RenameFlags, renameat_with};
use trash::TrashItem;

use super::{RestoreDisposition, TrashError, TrashReceipt, platform};
use crate::directory::{EntryKind, EntrySourceIdentity};

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TrashEntry {
    info: FileStamp,
    payload: FileStamp,
}

impl TrashEntry {
    fn read(info_path: &Path, payload_path: &Path) -> io::Result<Self> {
        let info = fs::symlink_metadata(info_path)?;
        let payload = fs::symlink_metadata(payload_path)?;
        if !info.is_file() || !(payload.is_file() || payload.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Trash metadata or payload has an unexpected file type",
            ));
        }
        Ok(Self {
            info: FileStamp::from_metadata(&info),
            payload: FileStamp::from_metadata(&payload),
        })
    }
}

fn item_paths(item: &TrashItem) -> Result<(&Path, PathBuf), TrashError> {
    // Linux の trash::TrashItem::id は実ファイル名に .trashinfo を付けた info の絶対パス。
    let info = Path::new(&item.id);
    let Some(trash_root) = info.parent().and_then(Path::parent) else {
        return Err(platform("Trash receipt has no trash directory"));
    };
    let Some(name) = info.file_stem() else {
        return Err(platform("Trash receipt has no payload name"));
    };
    if !info.is_absolute() || info.extension().is_none_or(|ext| ext != "trashinfo") {
        return Err(platform("Trash receipt has an invalid info path"));
    }
    Ok((info, trash_root.join("files").join(name)))
}

pub(super) fn check_source(
    path: &Path,
    accepted: &EntrySourceIdentity,
) -> Result<fs::Metadata, TrashError> {
    let metadata = fs::symlink_metadata(path).map_err(platform)?;
    let kind_matches = match accepted.kind {
        EntryKind::File => metadata.is_file(),
        EntryKind::Folder => metadata.is_dir(),
    };
    if !kind_matches
        || EntrySourceIdentity::from_path_no_follow(path, accepted.kind).as_ref() != Some(accepted)
    {
        return Err(platform("Trash source identity changed or is unsupported"));
    }
    Ok(metadata)
}

fn list() -> Result<Vec<TrashItem>, TrashError> {
    // trash 5.2.5 は一部の壊れた .trashinfo を読むと panic する。
    std::panic::catch_unwind(::trash::os_limited::list)
        .map_err(|_| platform("Could not read the Trash listing"))?
        .map_err(platform)
}

pub(super) fn move_to_trash(
    path: &Path,
    accepted: &EntrySourceIdentity,
) -> Result<TrashReceipt, TrashError> {
    let name = path
        .file_name()
        .ok_or_else(|| platform("Trash source has no name"))?;
    let parent = path
        .parent()
        .ok_or_else(|| platform("Trash source has no parent"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let source = parent.canonicalize().map_err(platform)?.join(name);
    let before: HashSet<_> = list()?.into_iter().map(|item| item.id).collect();
    let source_metadata = check_source(&source, accepted)?;
    ::trash::delete(&source).map_err(platform)?;

    // ここから先は Trash への移動が済んでいる。receipt を得られなくても削除は繰り返さない。
    let receipt = discover_receipt(&source, &before, &source_metadata);
    receipt.map_err(|error| {
        TrashError::MutationUncertain(format!(
            "Moved {} to Trash. Undo is unavailable for this item because its receipt could not be confirmed. {error}",
            source.display(),
        ))
    })
}

fn discover_receipt(
    source: &Path,
    before: &HashSet<std::ffi::OsString>,
    source_metadata: &fs::Metadata,
) -> Result<TrashReceipt, String> {
    let mut matches = list()
        .map_err(|error| format!("{error:?}"))?
        .into_iter()
        .filter(|item| item.original_path() == source);
    let item = matches.next().ok_or("No Trash entry was found.")?;
    if matches.next().is_some() {
        return Err("Multiple Trash entries have the same original path.".into());
    }
    if before.contains(&item.id) || item.time_deleted == -1 {
        return Err("No new Trash entry with a deletion time was found.".into());
    }
    let (info, payload) = item_paths(&item).map_err(|error| format!("{error:?}"))?;
    let linux_entry = TrashEntry::read(info, &payload).map_err(|error| error.to_string())?;
    if linux_entry.payload.device == source_metadata.dev()
        && linux_entry.payload.inode != source_metadata.ino()
    {
        return Err("The Trash entry does not match the moved filesystem object.".into());
    }
    Ok(TrashReceipt { item, linux_entry })
}

pub(super) fn restore(receipt: TrashReceipt) -> Result<RestoreDisposition, TrashError> {
    let target = receipt.item.original_path();
    let (info, payload) = item_paths(&receipt.item)?;
    if TrashEntry::read(info, &payload).map_err(platform)? != receipt.linux_entry {
        return Err(platform("The confirmed Trash entry has changed"));
    }
    // trash::restore_all はプレースホルダー作成後に置換 rename する。復元先の占有を
    // rename と同時に確認し、直前に作られた既存ファイルも上書きしない。
    match renameat_with(CWD, &payload, CWD, &target, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => return Err(TrashError::RestoreCollision(target)),
        Err(error) => return Err(platform(error)),
    }
    let cleanup = fs::symlink_metadata(info).and_then(|metadata| {
        if FileStamp::from_metadata(&metadata) == receipt.linux_entry.info {
            fs::remove_file(info)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Trash info changed",
            ))
        }
    });
    match cleanup {
        Ok(()) => Ok(RestoreDisposition::Completed),
        Err(error) => Ok(RestoreDisposition::CompletedWithAnomaly(format!(
            "Restored {}. Trash metadata cleanup failed ({error}).",
            target.display(),
        ))),
    }
}

#[cfg(all(test, unix))]
mod tests;
