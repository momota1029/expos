//! Linux の session file を、同じ directory 内の同期済み一時ファイルから置換する。

use super::file::WriteFailure;
use super::{MAX_SNAPSHOT_BYTES, SessionPaths};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct SessionFile {
    paths: SessionPaths,
}

impl SessionFile {
    pub(crate) const WRITES_SUPPORTED: bool = cfg!(target_os = "linux");

    pub(crate) fn new(paths: SessionPaths) -> Self {
        Self { paths }
    }

    #[allow(dead_code)] // テストから置換先を確かめる。
    pub(crate) fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    pub(crate) fn replace(&self, bytes: &[u8]) -> Result<(), WriteFailure> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(WriteFailure::Temporary("snapshot is too large".into()));
        }
        let directory = parent_directory(&self.paths.primary)
            .ok_or_else(|| WriteFailure::Temporary("primary has no directory".into()))?;
        ensure_private_directory(directory)?;
        let directory_file = File::open(directory)
            .map_err(|error| WriteFailure::Temporary(format!("open {directory:?}: {error}")))?;
        // Application 内の writer に加え、別 process による backup と primary の置換も直列化する。
        directory_file
            .lock()
            .map_err(|error| WriteFailure::Temporary(format!("lock {directory:?}: {error}")))?;

        let temporary = write_temporary(directory, |file| file.write_all(bytes))?;
        let publish = self
            .backup_primary(directory, &directory_file)
            .and_then(|()| rename(&temporary, &self.paths.primary));
        if let Err(failure) = publish {
            return Err(cleanup_temporary(&temporary, failure));
        }
        sync_directory(directory, &directory_file)
    }

    pub(crate) fn read_primary(&self) -> Result<Option<Vec<u8>>, String> {
        read_if_exists(&self.paths.primary)
    }

    pub(crate) fn read_backup(&self) -> Result<Option<Vec<u8>>, String> {
        read_if_exists(&self.paths.backup)
    }

    fn backup_primary(&self, directory: &Path, directory_file: &File) -> Result<(), WriteFailure> {
        let mut primary = match File::open(&self.paths.primary) {
            Ok(file) => file,
            // Windows と同じく、初回作成時は既存の backup に手を触れない。
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(WriteFailure::Temporary(format!(
                    "open {:?} for backup: {error}",
                    self.paths.primary
                )));
            }
        };
        let backup = write_temporary(directory, |file| io::copy(&mut primary, file).map(|_| ()))?;
        if let Err(failure) = rename(&backup, &self.paths.backup) {
            return Err(cleanup_temporary(&backup, failure));
        }
        // primary を置換する前に旧内容への名前を確定させ、電源断でも backup を辿れるようにする。
        sync_directory(directory, directory_file)
    }
}

fn parent_directory(path: &Path) -> Option<&Path> {
    path.parent().map(|parent| {
        if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        }
    })
}

fn ensure_private_directory(directory: &Path) -> Result<(), WriteFailure> {
    if directory.is_dir() {
        return Ok(());
    }
    let parent = parent_directory(directory)
        .filter(|parent| *parent != directory)
        .ok_or_else(|| WriteFailure::Temporary(format!("no parent for {directory:?}")))?;
    ensure_private_directory(parent)?;
    match DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && directory.is_dir() => {}
        Err(error) => {
            return Err(WriteFailure::Temporary(format!(
                "create {directory:?}: {error}"
            )));
        }
    }
    // 新しく作った directory の名前は、その親を同期しないと永続化されない。
    let parent_file = File::open(parent)
        .map_err(|error| WriteFailure::SyncDirectory(format!("open {parent:?}: {error}")))?;
    sync_directory(parent, &parent_file)
}

fn write_temporary(
    directory: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<PathBuf, WriteFailure> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let (path, mut file) = loop {
        let path = directory.join(format!(
            ".session-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => break (path, file),
            // PID 再利用で残った一時ファイルと衝突しても、そのファイルには触れない。
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(WriteFailure::Temporary(format!("create {path:?}: {error}")));
            }
        }
    };
    if let Err(error) = write(&mut file).and_then(|()| file.sync_all()) {
        let failure = WriteFailure::Temporary(format!("write or sync {path:?}: {error}"));
        return Err(cleanup_temporary(&path, failure));
    }
    Ok(path)
}

fn rename(from: &Path, to: &Path) -> Result<(), WriteFailure> {
    fs::rename(from, to).map_err(|error| WriteFailure::Replace {
        code: error.raw_os_error().map(|code| code as u32),
        message: format!("rename {from:?} to {to:?}: {error}"),
    })
}

fn sync_directory(path: &Path, file: &File) -> Result<(), WriteFailure> {
    file.sync_all()
        .map_err(|error| WriteFailure::SyncDirectory(format!("sync {path:?}: {error}")))
}

fn cleanup_temporary(path: &Path, failure: WriteFailure) -> WriteFailure {
    match fs::remove_file(path) {
        Ok(()) => failure,
        Err(error) => WriteFailure::Cleanup(format!(
            "{failure}; and {path:?} could not be removed: {error}"
        )),
    }
}

fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {path:?}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn replace_reopens_with_the_previous_primary_as_backup() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(root.path().join("session.json"));
        let file = SessionFile::new(paths.clone());
        assert_eq!(file.read_primary().unwrap(), None);
        file.replace(b"first").unwrap();
        assert_eq!(file.read_backup().unwrap(), None);
        file.replace(b"second").unwrap();
        file.replace(b"third").unwrap();
        drop(file);
        let reopened = SessionFile::new(paths);
        assert_eq!(
            reopened.read_primary().unwrap().as_deref(),
            Some(&b"third"[..])
        );
        assert_eq!(
            reopened.read_backup().unwrap().as_deref(),
            Some(&b"second"[..])
        );
    }

    #[test]
    fn writing_with_no_primary_keeps_an_existing_backup() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(root.path().join("session.json"));
        fs::write(&paths.backup, b"existing backup").unwrap();
        let file = SessionFile::new(paths);
        file.replace(b"new primary").unwrap();
        assert_eq!(
            file.read_backup().unwrap().as_deref(),
            Some(&b"existing backup"[..])
        );
    }

    #[test]
    fn backup_publish_failure_keeps_the_primary_and_cleans_its_temps() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(root.path().join("session.json"));
        let file = SessionFile::new(paths.clone());
        file.replace(b"previous").unwrap();
        fs::create_dir(&paths.backup).unwrap();

        assert!(matches!(
            file.replace(b"next"),
            Err(WriteFailure::Replace { .. })
        ));
        assert_eq!(
            file.read_primary().unwrap().as_deref(),
            Some(&b"previous"[..])
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn created_directories_and_files_are_private() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("expos");
        let directory = state.join("session");
        let paths = SessionPaths::new(directory.join("session.json"));
        let file = SessionFile::new(paths.clone());
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        file.replace(b"first").unwrap();
        fs::set_permissions(&paths.primary, fs::Permissions::from_mode(0o644)).unwrap();
        file.replace(b"second").unwrap();
        let staged = write_temporary(&directory, |_| Ok(())).unwrap();
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&directory), 0o700);
        assert_eq!(mode(&paths.primary), 0o600);
        assert_eq!(mode(&paths.backup), 0o600);
        assert_eq!(mode(&staged), 0o600);
    }

    #[test]
    fn concurrent_replacements_keep_the_last_two_contents() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(root.path().join("session.json"));
        let file = SessionFile::new(paths.clone());
        file.replace(b"initial").unwrap();
        let other = SessionFile::new(paths);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| file.replace(b"first").unwrap());
            other.replace(b"second").unwrap();
            first.join().unwrap();
        });
        let mut contents = [file.read_primary().unwrap(), file.read_backup().unwrap()];
        contents.sort();
        assert_eq!(
            contents,
            [Some(b"first".to_vec()), Some(b"second".to_vec())]
        );
    }
}
