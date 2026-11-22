//! Session storage fallback for targets without a Windows or Unix backend.
//! Reads behave like a first run. Writes fail before touching the filesystem.

use super::SessionPaths;
use super::file::WriteFailure;

pub(crate) struct SessionFile {
    paths: SessionPaths,
}

impl SessionFile {
    pub(crate) const WRITES_SUPPORTED: bool = false;

    pub(crate) fn new(paths: SessionPaths) -> Self {
        Self { paths }
    }

    #[allow(dead_code)] // Journal tests inspect the configured primary path.
    pub(crate) fn paths(&self) -> &SessionPaths {
        &self.paths
    }

    pub(crate) fn replace(&self, _bytes: &[u8]) -> Result<(), WriteFailure> {
        Err(WriteFailure::Unsupported)
    }

    pub(crate) fn read_primary(&self) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }

    pub(crate) fn read_backup(&self) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
}
