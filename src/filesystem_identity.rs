/// Opaque filesystem-object identity shared by directory observations and
/// immutable operation requests.
///
/// The value deliberately carries no path. A path names where to look; this
/// token identifies the object observed there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FilesystemIdentity(String);

impl FilesystemIdentity {
    pub(crate) fn from_windows_parts(volume_serial: u32, file_index: u64) -> Self {
        Self(format!("{:08x}:{file_index:016x}", volume_serial))
    }

    pub(crate) fn windows_parts(&self) -> Option<(u32, u64)> {
        let (volume, index) = self.0.split_once(':')?;
        if volume.len() != 8 || index.len() != 16 {
            return None;
        }
        Some((
            u32::from_str_radix(volume, 16).ok()?,
            u64::from_str_radix(index, 16).ok()?,
        ))
    }

    /// Returns only the stable Windows identity fields for bounded helper IPC.
    /// Fallback identities intentionally have no cross-process representation.
    pub(crate) fn helper_transport_parts(&self) -> Option<(u32, u64)> {
        self.windows_parts()
    }

    pub(crate) fn from_helper_transport_parts(volume: u32, index: u64) -> Self {
        Self::from_windows_parts(volume, index)
    }

    #[cfg(any(not(target_os = "windows"), test))]
    pub(crate) fn from_fallback_token(token: String) -> Self {
        Self(format!("other:{token}"))
    }
}

#[cfg(test)]
mod tests {
    use super::FilesystemIdentity;

    #[test]
    fn windows_identity_round_trips_its_file_id() {
        let identity = FilesystemIdentity::from_windows_parts(0x1234_abcd, 0x0123_4567_89ab_cdef);
        assert_eq!(
            identity.windows_parts(),
            Some((0x1234_abcd, 0x0123_4567_89ab_cdef))
        );
    }

    #[test]
    fn fallback_and_malformed_identities_do_not_become_drag_sources() {
        assert_eq!(
            FilesystemIdentity::from_fallback_token("source".into()).windows_parts(),
            None
        );
        assert_eq!(FilesystemIdentity("1234:abcd".into()).windows_parts(), None);
        assert_eq!(
            FilesystemIdentity("zzzzzzzz:0000000000000000".into()).windows_parts(),
            None
        );
    }
}
