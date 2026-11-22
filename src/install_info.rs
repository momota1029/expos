use std::path::PathBuf;

pub(crate) fn install_root() -> Option<PathBuf> {
    std::env::var_os("EXPOS_INSTALL_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn version_dir() -> Option<PathBuf> {
    std::env::var_os("EXPOS_VERSION_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
