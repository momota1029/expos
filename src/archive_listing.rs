//! Bounded adapter between an ordinary ZIP source and detached directory rows.
//! Parsing stays in `zip_archive`; this module holds the Windows source/route proof
//! until the owning `DirectorySession` has made its commit decision.

use crate::{
    archive_manifest::{ArchiveManifest, NodeKind, RawRecord},
    directory::Entry,
    location::Location,
    zip_archive,
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub(crate) struct ArchiveReadResult {
    pub(crate) manifest: ArchiveManifest,
    _lease: SourceRouteLease,
}

pub(crate) struct ArchiveReadCustody {
    _lease: SourceRouteLease,
}

impl ArchiveReadResult {
    pub(crate) fn into_parts(self) -> (ArchiveManifest, ArchiveReadCustody) {
        (
            self.manifest,
            ArchiveReadCustody {
                _lease: self._lease,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn test_result(manifest: ArchiveManifest) -> Self {
        Self {
            manifest,
            _lease: SourceRouteLease {
                _source: tempfile::tempfile().expect("test custody file"),
                _ancestors: Vec::new(),
            },
        }
    }
}

struct SourceRouteLease {
    _source: File,
    _ancestors: Vec<File>,
}

pub(crate) fn read(
    location: &Location,
    cancelled: &AtomicBool,
) -> Result<ArchiveReadResult, crate::directory::DirectoryError> {
    let Location::Archive { source, components } = location else {
        return Err(crate::directory::DirectoryError(
            "archive reader received a non-archive location".into(),
        ));
    };
    #[cfg(windows)]
    {
        let mut lease = open_source_route(source)?;
        let before = source_signature(&lease._source)?;
        let zip_manifest = zip_archive::inspect(&mut lease._source, cancelled)
            .map_err(|error| crate::directory::DirectoryError(error.to_string()))?;
        let manifest = from_zip_nodes(&zip_manifest.nodes)?;
        validate_requested_directory(components, &manifest)?;
        let after = source_signature(&lease._source)?;
        let reopened = open_source_file(source)?;
        if before != after || after != source_signature(&reopened)? {
            return Err(crate::directory::DirectoryError(
                "archive source changed during read".into(),
            ));
        }
        Ok(ArchiveReadResult {
            manifest,
            _lease: lease,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        Err(crate::directory::DirectoryError(
            "archive browsing requires Windows file identity".into(),
        ))
    }
}

#[cfg(windows)]
pub(crate) fn read_7z(
    owner: &crate::archive_provider::ArchiveProviderOwner,
    capability: &crate::archive_provider::Archive7zReadCapability,
    image: crate::archive_provider::application_image::ApplicationImageLease,
    location: &Location,
    cancelled: &AtomicBool,
) -> Result<ArchiveReadResult, crate::directory::DirectoryError> {
    let Location::Archive { source, components } = location else {
        return Err(crate::directory::DirectoryError(
            "archive reader received a non-archive location".into(),
        ));
    };
    let lease = open_source_route(source)?;
    let before = source_signature(&lease._source)?;
    let cwd = source.parent().ok_or_else(|| {
        crate::directory::DirectoryError("archive source has no parent directory".into())
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let manifest = owner
        .read_7z_archive(capability, image, &lease._source, cwd, deadline, cancelled)
        .map_err(|outcome| {
            crate::directory::DirectoryError(format!("7z metadata read failed: {outcome:?}"))
        })?;
    validate_requested_directory(components, &manifest)?;
    let after = source_signature(&lease._source)?;
    let reopened = open_source_file(source)?;
    if before != after || after != source_signature(&reopened)? {
        return Err(crate::directory::DirectoryError(
            "archive source changed during read".into(),
        ));
    }
    Ok(ArchiveReadResult {
        manifest,
        _lease: lease,
    })
}

fn validate_requested_directory(
    components: &[String],
    manifest: &ArchiveManifest,
) -> Result<(), crate::directory::DirectoryError> {
    if components.is_empty()
        || manifest
            .nodes
            .iter()
            .any(|node| node.components == components && node.kind == NodeKind::Directory)
    {
        Ok(())
    } else {
        Err(crate::directory::DirectoryError(
            "archive directory does not exist".into(),
        ))
    }
}

fn from_zip_nodes(
    nodes: &[zip_archive::ManifestNode],
) -> Result<ArchiveManifest, crate::directory::DirectoryError> {
    ArchiveManifest::validate(
        nodes
            .iter()
            .filter(|node| node.explicit)
            .map(|node| RawRecord {
                components: node.components.clone(),
                kind: match node.kind {
                    zip_archive::EntryKind::File => NodeKind::File,
                    zip_archive::EntryKind::Directory => NodeKind::Directory,
                },
                declared_size: node.declared_uncompressed_size,
            }),
    )
    .map_err(|error| crate::directory::DirectoryError(error.to_string()))
}

/// Build only the direct children of the current typed address. Entries remain
/// pathless and their key retains the source plus archive-relative identity.
pub(crate) fn project_children(
    source: &Path,
    parent: &[String],
    manifest: &ArchiveManifest,
) -> Vec<Entry> {
    manifest
        .nodes
        .iter()
        .filter_map(|node| {
            if node.components.len() != parent.len() + 1 || !node.components.starts_with(parent) {
                return None;
            }
            let name = node.components.last()?.clone();
            let kind = match node.kind {
                NodeKind::File => crate::directory::EntryKind::File,
                NodeKind::Directory => crate::directory::EntryKind::Folder,
            };
            Some(Entry {
                key: crate::entry_identity::EntryKey::Archive {
                    source: source.to_path_buf(),
                    components: node.components.clone(),
                    kind,
                },
                name,
                path: None,
                kind,
                link: None,
                link_target: None,
                source_identity: None,
                is_hidden: false,
                size: node.declared_size,
                folder_size: None,
                modified: None,
            })
        })
        .collect()
}

#[cfg(windows)]
fn open_source_route(source: &Path) -> Result<SourceRouteLease, crate::directory::DirectoryError> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };
    if !source.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("7z")
    }) {
        return Err(crate::directory::DirectoryError(
            "only ordinary .zip or .7z files can be browsed".into(),
        ));
    }
    if !source.is_absolute() {
        return Err(crate::directory::DirectoryError(
            "archive source path must be absolute".into(),
        ));
    }
    let mut ancestors = Vec::new();
    let mut prefix = PathBuf::new();
    for component in source.parent().into_iter().flat_map(Path::components) {
        prefix.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&prefix)
            .map_err(|error| crate::directory::DirectoryError(error.to_string()))?;
        let metadata = handle
            .metadata()
            .map_err(|error| crate::directory::DirectoryError(error.to_string()))?;
        if metadata.file_attributes()
            & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
            != FILE_ATTRIBUTE_DIRECTORY.0
        {
            return Err(crate::directory::DirectoryError(
                "archive route contains a non-ordinary directory".into(),
            ));
        }
        ancestors.push(handle);
        let resolved = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(&prefix)
            .map_err(|error| crate::directory::DirectoryError(error.to_string()))?;
        let resolved_attributes = resolved
            .metadata()
            .map_err(|error| crate::directory::DirectoryError(error.to_string()))?
            .file_attributes();
        if resolved_attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || resolved_attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(crate::directory::DirectoryError(
                "archive route did not resolve to an ordinary directory".into(),
            ));
        }
        ancestors.push(resolved);
    }
    let source_file = open_source_file(source)?;
    let attributes = source_file
        .metadata()
        .map_err(|error| crate::directory::DirectoryError(error.to_string()))?
        .file_attributes();
    if attributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) != 0 {
        return Err(crate::directory::DirectoryError(
            "archive source is not an ordinary file".into(),
        ));
    }
    Ok(SourceRouteLease {
        _source: source_file,
        _ancestors: ancestors,
    })
}

#[cfg(windows)]
fn open_source_file(source: &Path) -> Result<File, crate::directory::DirectoryError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(source)
        .map_err(|error| crate::directory::DirectoryError(error.to_string()))
}

#[cfg(windows)]
fn source_signature(file: &File) -> Result<(u32, u64, u64, u64), crate::directory::DirectoryError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut info) }
        .map_err(|error| crate::directory::DirectoryError(error.to_string()))?;
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let size = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
    let modified = (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
        | u64::from(info.ftLastWriteTime.dwLowDateTime);
    Ok((info.dwVolumeSerialNumber, index, size, modified))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn projection_returns_only_immediate_typed_pathless_rows() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("items.zip");
        let mut writer = zip::ZipWriter::new(File::create(&source).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        writer.add_directory("empty/", options).unwrap();
        writer.start_file("nested/child.txt", options).unwrap();
        writer.write_all(b"content").unwrap();
        writer.finish().unwrap();
        let zip_manifest =
            zip_archive::inspect(&mut File::open(&source).unwrap(), &AtomicBool::new(false))
                .unwrap();
        let manifest = from_zip_nodes(&zip_manifest.nodes).unwrap();
        let zip_facts = zip_manifest
            .nodes
            .iter()
            .map(|node| {
                (
                    node.components.clone(),
                    node.kind,
                    node.explicit,
                    node.declared_uncompressed_size,
                )
            })
            .collect::<Vec<_>>();
        let browsing_facts = manifest
            .nodes
            .iter()
            .map(|node| {
                (
                    node.components.clone(),
                    match node.kind {
                        NodeKind::File => zip_archive::EntryKind::File,
                        NodeKind::Directory => zip_archive::EntryKind::Directory,
                    },
                    node.explicit,
                    node.declared_size,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(browsing_facts, zip_facts);
        let rows = project_children(&source, &[], &manifest);
        assert_eq!(
            rows.iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["empty", "nested"]
        );
        assert!(rows.iter().all(|entry| entry.path.is_none()));
        assert!(
            rows.iter()
                .all(|entry| matches!(entry.key, crate::entry_identity::EntryKey::Archive { .. }))
        );
        let nested = project_children(&source, &["nested".into()], &manifest);
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].name, "child.txt");
    }

    #[test]
    fn requested_empty_directory_exists_but_missing_directory_fails() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("directories.zip");
        let mut writer = zip::ZipWriter::new(File::create(&source).unwrap());
        writer
            .add_directory("empty/", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();
        let manifest =
            zip_archive::inspect(&mut File::open(&source).unwrap(), &AtomicBool::new(false))
                .unwrap();
        let manifest = from_zip_nodes(&manifest.nodes).unwrap();
        assert!(validate_requested_directory(&["empty".into()], &manifest).is_ok());
        assert!(validate_requested_directory(&["missing".into()], &manifest).is_err());
        assert!(validate_requested_directory(&[], &manifest).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn read_fails_closed_when_an_ancestor_is_a_junction() {
        use std::time::{SystemTime, UNIX_EPOCH};

        struct FixtureCleanup {
            root: PathBuf,
            junction: PathBuf,
        }
        impl Drop for FixtureCleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(&self.junction);
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        // Junction は同一 volume が必要なので、target 配下に一時 fixture を置く。
        let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/archive-listing-fixtures");
        std::fs::create_dir_all(&fixture_root).unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = fixture_root.join(format!(
            "gpui-filer-archive-junction-{}-{unique}",
            std::process::id()
        ));
        let target = root.join("target");
        let junction = root.join("route");
        let _cleanup = FixtureCleanup {
            root: root.clone(),
            junction: junction.clone(),
        };
        std::fs::create_dir_all(&target).unwrap();
        let archive = target.join("items.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .start_file("member.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"content").unwrap();
        writer.finish().unwrap();

        crate::operations::privileged::create_junction_direct(&target, &junction)
            .expect("create real junction fixture");
        let location = Location::Archive {
            source: junction.join("items.zip"),
            components: Vec::new(),
        };
        let result = read(&location, &AtomicBool::new(false));
        assert!(
            matches!(
                result,
                Err(crate::directory::DirectoryError(message))
                    if message == "archive route contains a non-ordinary directory"
            ),
            "junction ancestor must hit the no-reparse route guard"
        );
    }

    #[cfg(windows)]
    #[test]
    fn source_route_lease_lives_through_session_decision_then_releases() {
        use crate::directory_session::{DirectorySession, SessionEffect};
        use std::fs;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("items.zip");
        let replacement = root.path().join("replacement.zip");
        let mut writer = zip::ZipWriter::new(File::create(&source).unwrap());
        writer
            .start_file("member.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"content").unwrap();
        writer.finish().unwrap();

        let location = Location::Archive {
            source: source.clone(),
            components: vec![],
        };
        let mut session = DirectorySession::default();
        let ticket = session
            .navigate(location.clone(), None)
            .into_iter()
            .find_map(|effect| match effect {
                SessionEffect::EnumerateArchive(ticket) => Some(ticket),
                _ => None,
            })
            .unwrap();
        let result = read(&location, &AtomicBool::new(false)).unwrap();
        assert!(fs::rename(&source, &replacement).is_err());
        assert!(fs::OpenOptions::new().write(true).open(&source).is_err());
        assert!(session.commit_archive(&ticket, Ok(result)));
        let accepted_nodes = session.archive_manifest().unwrap().nodes.len();
        let refresh = session.refresh().unwrap();
        let refresh_ticket = match refresh {
            SessionEffect::EnumerateArchive(ticket) => ticket,
            other => panic!("expected archive refresh, got {other:?}"),
        };
        assert!(session.commit_archive(
            &refresh_ticket,
            Err(crate::directory::DirectoryError("failed refresh".into()))
        ));
        assert!(matches!(
            session.status(),
            crate::directory_session::ListingStatus::RefreshFailed(_)
        ));
        assert_eq!(
            session.archive_manifest().unwrap().nodes.len(),
            accepted_nodes
        );
        fs::rename(&source, &replacement).unwrap();

        let stale_location = Location::Archive {
            source: replacement.clone(),
            components: vec![],
        };
        let stale_ticket = session
            .navigate(stale_location.clone(), None)
            .into_iter()
            .find_map(|effect| match effect {
                SessionEffect::EnumerateArchive(ticket) => Some(ticket),
                _ => None,
            })
            .unwrap();
        let stale_result = read(&stale_location, &AtomicBool::new(false)).unwrap();
        session.navigate(
            Location::Archive {
                source: root.path().join("other.zip"),
                components: vec![],
            },
            None,
        );
        assert!(!session.commit_archive(&stale_ticket, Ok(stale_result)));
        fs::rename(&replacement, &source).unwrap();
    }
}
