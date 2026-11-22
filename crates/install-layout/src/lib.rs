//! Filesystem contract for the Windows installer, launcher, and updater.

use semver::Version;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{self, Path, PathBuf};
use std::process::{Child, Command};

#[cfg(windows)]
mod windows;

pub const INSTALL_ROOT_ENV: &str = "EXPOS_INSTALL_ROOT";
pub const VERSION_DIR_ENV: &str = "EXPOS_VERSION_DIR";
const APP_EXE: &str = "expos.exe";
const CURRENT: &str = "current";
const LOCK: &str = ".lock";
const MAX_DIR_NAME: usize = 255;

/// Resolve the absolute installation path without creating it.
pub fn install_root() -> io::Result<PathBuf> {
    resolve_install_root(
        env::var_os(INSTALL_ROOT_ENV).as_deref(),
        env::var_os("LOCALAPPDATA").as_deref(),
    )
}

fn resolve_install_root(
    override_root: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
) -> io::Result<PathBuf> {
    if let Some(root) = override_root.filter(|value| !value.is_empty()) {
        return path::absolute(root);
    }
    let local = local_app_data
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
    path::absolute(Path::new(local).join("expos"))
}

/// A validated directory name of the form `v<semver>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VersionDir(Version);

impl VersionDir {
    pub fn parse(name: &str) -> Option<Self> {
        Self::from_version(name.strip_prefix('v')?)
    }

    pub fn from_version(version: &str) -> Option<Self> {
        if version.len() >= MAX_DIR_NAME {
            return None;
        }
        // SemVer の文字集合にパス区切り、ドライブ記号、空の識別子は含まれない。
        Version::parse(version).ok().map(Self)
    }

    pub fn name(&self) -> String {
        format!("v{}", self.0)
    }

    pub fn version(&self) -> &Version {
        &self.0
    }

    pub fn path(&self, root: &Path) -> PathBuf {
        root.join(self.name())
    }

    pub fn executable(&self, root: &Path) -> PathBuf {
        self.path(root).join(APP_EXE)
    }
}

/// Read the single-line pointer. A missing or malformed pointer returns `None`.
pub fn read_current(root: &Path) -> io::Result<Option<VersionDir>> {
    let pointer = root.join(CURRENT);
    if !ordinary_file(&pointer)? {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    match File::open(pointer) {
        Ok(file) => file
            .take((MAX_DIR_NAME + 3) as u64)
            .read_to_end(&mut bytes)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(std::str::from_utf8(&bytes).ok().and_then(|text| {
        let name = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        VersionDir::parse(name)
    }))
}

/// List complete payload directories, newest first. Build metadata breaks SemVer ties.
pub fn list_installed_versions(root: &Path) -> io::Result<Vec<VersionDir>> {
    let mut versions = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if let Some(version) = entry.file_name().to_str().and_then(VersionDir::parse) {
            if is_installed(root, &version)? {
                versions.push(version);
            }
        }
    }
    versions.sort_unstable_by(|a, b| b.cmp(a));
    Ok(versions)
}

pub fn select_version(root: &Path) -> io::Result<Option<VersionDir>> {
    if let Some(current) = read_current(root)? {
        if is_installed(root, &current)? {
            return Ok(Some(current));
        }
    }
    Ok(list_installed_versions(root)?.into_iter().next())
}

fn is_installed(root: &Path, version: &VersionDir) -> io::Result<bool> {
    Ok(
        ordinary_metadata(&version.path(root))?.is_some_and(|meta| meta.is_dir())
            && ordinary_file(&version.executable(root))?,
    )
}

fn ordinary_file(path: &Path) -> io::Result<bool> {
    Ok(ordinary_metadata(path)?.is_some_and(|meta| meta.is_file()))
}

fn ordinary_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => Ok(None),
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use ::windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

/// A held installation lock. Keep it through payload publication, activation, and pruning.
pub struct InstallLock {
    root: PathBuf,
    _file: File,
}

/// Lock an existing install root until the guard is dropped.
///
/// Windows participants lock byte zero of the persistent `.lock` file.
pub fn lock_install_root(root: &Path) -> io::Result<InstallLock> {
    let root = path::absolute(root)?;
    if !ordinary_metadata(&root)?.is_some_and(|meta| meta.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "install directory is missing or is a link: {}",
                root.display()
            ),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    let file = windows::lock(&root.join(LOCK), options)?;
    #[cfg(not(windows))]
    let file = {
        let file = options.open(root.join(LOCK))?;
        file.lock()?;
        file
    };
    Ok(InstallLock { root, _file: file })
}

impl InstallLock {
    /// Replace `current` with the name of a complete payload in this root.
    pub fn activate(&self, version: &VersionDir) -> io::Result<()> {
        if !is_installed(&self.root, version)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} has no installed {APP_EXE}",
                    version.path(&self.root).display()
                ),
            ));
        }
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        writeln!(temp, "{}", version.name())?;
        temp.as_file().sync_all()?;
        let temp = temp.into_temp_path();
        #[cfg(windows)]
        return windows::replace(&temp, &self.root.join(CURRENT));
        #[cfg(not(windows))]
        fs::rename(&temp, self.root.join(CURRENT))
    }

    /// Check whether another process holds the payload executable open.
    pub fn is_version_in_use(&self, version: &VersionDir) -> io::Result<bool> {
        let exe = version.executable(&self.root);
        #[cfg(windows)]
        return windows::is_in_use(&exe);
        #[cfg(not(windows))]
        {
            let file = File::open(exe)?;
            match file.try_lock() {
                Ok(()) => Ok(false),
                Err(fs::TryLockError::WouldBlock) => Ok(true),
                Err(fs::TryLockError::Error(err)) => Err(err),
            }
        }
    }

    /// Remove complete versions other than the newest `keep_newest`, current, and versions in use.
    pub fn prune(&self, keep_newest: usize) -> io::Result<Vec<VersionDir>> {
        let current = read_current(&self.root)?;
        let mut removed = Vec::new();
        for version in list_installed_versions(&self.root)?
            .into_iter()
            .skip(keep_newest)
        {
            if current
                .as_ref()
                .is_some_and(|current| same_name(current, &version))
                || self.is_version_in_use(&version)?
            {
                continue;
            }
            fs::remove_dir_all(version.path(&self.root))?;
            removed.push(version);
        }
        Ok(removed)
    }

    /// Spawn a payload under the install lock. The Windows child keeps its executable pinned.
    pub fn spawn_version(
        &self,
        version: &VersionDir,
        configure: impl FnOnce(&mut Command),
    ) -> io::Result<Child> {
        let exe = version.executable(&self.root);
        let mut command = Command::new(&exe);
        configure(&mut command);
        #[cfg(windows)]
        return windows::spawn_pinned(command, &exe);
        #[cfg(not(windows))]
        command.spawn()
    }
}

fn same_name(a: &VersionDir, b: &VersionDir) -> bool {
    #[cfg(windows)]
    return a.name().eq_ignore_ascii_case(&b.name());
    #[cfg(not(windows))]
    {
        a == b
    }
}

#[cfg(test)]
mod tests;
