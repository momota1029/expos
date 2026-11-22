use std::fs::{self, Metadata};
use std::io;
use std::path::Path;

use crate::channel::Channel;

mod apply;
mod selection;
mod settings;
mod source;
#[cfg(windows)]
pub(crate) mod ui;

use selection::Offer;

fn ordinary_file(path: &Path) -> io::Result<bool> {
    Ok(ordinary_metadata(path)?.is_some_and(|meta| meta.is_file()))
}

fn ordinary_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update path is a link or reparse point",
        )),
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}
