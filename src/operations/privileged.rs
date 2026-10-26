//! A deliberately small, session-scoped elevated operation broker.
//!
//! An ordinary filer process never inherits elevation.  When an operation has
//! already proved that it needs `SeCreateSymbolicLinkPrivilege`, it starts the
//! same executable in the broker mode below.  The broker owns no directory
//! state and accepts only authenticated, loopback requests.  It exits on an
//! authenticated shutdown request or after a short idle period, so a crashed
//! normal process cannot leave an administrator process around indefinitely.

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

const BROKER_ARGUMENT: &str = "--gpui-filer-privileged-broker";
const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const CONNECT_RETRY: Duration = Duration::from_millis(50);
const START_TIMEOUT: Duration = Duration::from_secs(45);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Shared by operation workers in one application session.  It contains only
/// an opaque endpoint capability; filesystem intent stays in `OperationRequest`.
#[derive(Clone, Default)]
pub struct PrivilegedBroker {
    state: Arc<Mutex<BrokerState>>,
}

/// A direct observation of the reparse object.  This is intentionally more
/// specific than `FileType::is_symlink()`: on Windows that predicate also
/// accepts junctions, which must not silently become symbolic links on copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkObjectKind {
    SymbolicLink { directory: bool },
    Junction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkMoveResult {
    Completed,
    Partial(String),
}

/// One normal filer process owns one broker session.  The process-wide holder
/// lets separate operation workers reuse the approval without smuggling a
/// mutable process handle through immutable operation requests.
static SESSION_BROKER: OnceLock<PrivilegedBroker> = OnceLock::new();

pub fn session_broker() -> &'static PrivilegedBroker {
    SESSION_BROKER.get_or_init(PrivilegedBroker::default)
}

/// Called by the shell's orderly shutdown path.  A process crash still leaves
/// only the broker's bounded idle timeout.
pub fn shutdown_session_broker() {
    if let Some(broker) = SESSION_BROKER.get() {
        broker.shutdown();
    }
}

#[derive(Default)]
struct BrokerState {
    endpoint: Option<Endpoint>,
}

impl Drop for BrokerState {
    fn drop(&mut self) {
        if let Some(endpoint) = self.endpoint.take() {
            // A normal clean exit shuts the elevated process down immediately.
            // The broker also has an idle timeout for process-crash recovery.
            let _ = endpoint.request(BrokerCommand::Shutdown, Duration::from_millis(250));
        }
    }
}

#[derive(Clone)]
struct Endpoint {
    address: SocketAddr,
    token: String,
}

impl PrivilegedBroker {
    /// Creates the link through the already-approved broker.  Starting the
    /// broker happens on the operation worker, never the GPUI event thread.
    pub fn create_symbolic_link(
        &self,
        source: &Path,
        destination: &Path,
        expected_parent_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), String> {
        let endpoint = self.endpoint_or_start(cancelled)?;
        let (expected_parent_volume, expected_parent_file_index) = match expected_parent_identity {
            Some(identity) => {
                let (volume, index) = identity.windows_parts().ok_or_else(|| {
                    "symbolic-link destination identity is not transportable".to_owned()
                })?;
                (Some(volume), Some(index))
            }
            None => (None, None),
        };
        let command = BrokerCommand::CreateSymbolicLink {
            source: WirePath::from_path(source),
            destination: WirePath::from_path(destination),
            expected_parent_volume,
            expected_parent_file_index,
        };
        // Foreign Linkのterminal resultを返す前にbrokerのpublication完了を受け取る。
        // response timeoutでclient leaseだけを落とすと、brokerが後からlinkを作れてしまう。
        let reply = if expected_parent_identity.is_some() {
            endpoint.request_until_reply(command, CLIENT_TIMEOUT)?
        } else {
            endpoint.request(command, CLIENT_TIMEOUT)?
        };
        match reply {
            BrokerReply::Done => Ok(()),
            BrokerReply::Error { detail } => Err(detail),
            BrokerReply::Pong | BrokerReply::Partial { .. } => {
                Err("privileged broker returned an invalid link reply".into())
            }
        }
    }

    pub fn create_junction(
        &self,
        target: &Path,
        destination: &Path,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), String> {
        let endpoint = self.endpoint_or_start(cancelled)?;
        match endpoint.request(
            BrokerCommand::CreateJunction {
                target: WirePath::from_path(target),
                destination: WirePath::from_path(destination),
            },
            CLIENT_TIMEOUT,
        )? {
            BrokerReply::Done => Ok(()),
            BrokerReply::Error { detail } => Err(detail),
            BrokerReply::Pong | BrokerReply::Partial { .. } => {
                Err("privileged broker returned an invalid junction reply".into())
            }
        }
    }

    /// Re-runs the ordinary no-clobber adapter with the broker's token.  This
    /// is intentionally limited to a real file; folders and reparse points
    /// retain their separate, explicit operation paths.
    pub fn copy_regular_file(
        &self,
        source: &Path,
        destination: &Path,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), String> {
        self.request_done(
            BrokerCommand::CopyRegularFile {
                source: WirePath::from_path(source),
                destination: WirePath::from_path(destination),
            },
            cancelled,
            "copy",
        )
    }

    /// Re-runs the same-volume/cross-volume move adapter with elevation.  It
    /// does not introduce a separate rename implementation in the broker.
    pub fn move_regular_file(
        &self,
        source: &Path,
        destination: &Path,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), String> {
        self.request_done(
            BrokerCommand::MoveRegularFile {
                source: WirePath::from_path(source),
                destination: WirePath::from_path(destination),
            },
            cancelled,
            "move",
        )
    }

    pub fn copy_link_object(
        &self,
        source: &Path,
        destination: &Path,
        cancelled: impl Fn() -> bool,
    ) -> Result<(), String> {
        self.request_done(
            BrokerCommand::CopyLinkObject {
                source: WirePath::from_path(source),
                destination: WirePath::from_path(destination),
            },
            cancelled,
            "link copy",
        )
    }

    pub fn move_link_object(
        &self,
        source: &Path,
        destination: &Path,
        cancelled: impl Fn() -> bool,
    ) -> Result<LinkMoveResult, String> {
        let endpoint = self.endpoint_or_start(cancelled)?;
        match endpoint.request(
            BrokerCommand::MoveLinkObject {
                source: WirePath::from_path(source),
                destination: WirePath::from_path(destination),
            },
            CLIENT_TIMEOUT,
        )? {
            BrokerReply::Done => Ok(LinkMoveResult::Completed),
            BrokerReply::Partial { detail } => Ok(LinkMoveResult::Partial(detail)),
            BrokerReply::Error { detail } => Err(detail),
            BrokerReply::Pong => {
                Err("privileged broker returned an invalid link move reply".into())
            }
        }
    }

    pub fn move_link_object_with_evidence(
        &self,
        source: &Path,
        destination: &Path,
        evidence: &crate::operations::model::ClipboardMoveSourceEvidence,
        destination_identity: &crate::filesystem_identity::FilesystemIdentity,
        cancelled: impl Fn() -> bool,
    ) -> Result<LinkMoveResult, String> {
        if evidence.source != source {
            return Err("clipboard source evidence path does not match broker request".into());
        }
        let Some((source_volume, source_file_index)) = evidence.source_identity.windows_parts() else {
            return Err("clipboard source identity is not a Windows file identity".into());
        };
        let Some(reparse_tag) = evidence.reparse_tag else {
            return Err("clipboard source evidence is not a reparse object".into());
        };
        let Some((destination_volume, destination_file_index)) = destination_identity.windows_parts() else {
            return Err("clipboard destination identity is not a Windows directory identity".into());
        };
        let endpoint = self.endpoint_or_start(cancelled)?;
        match endpoint.request(
            BrokerCommand::MoveLinkObjectWithEvidence {
                source: WirePath::from_path(source),
                destination: WirePath::from_path(destination),
                source_volume,
                source_file_index,
                reparse_tag,
                source_is_directory: evidence.source_kind == crate::directory::EntryKind::Folder,
                destination_volume,
                destination_file_index,
            },
            CLIENT_TIMEOUT,
        )? {
            BrokerReply::Done => Ok(LinkMoveResult::Completed),
            BrokerReply::Partial { detail } => Ok(LinkMoveResult::Partial(detail)),
            BrokerReply::Error { detail } => Err(detail),
            BrokerReply::Pong => Err("privileged broker returned an invalid identity-bound link move reply".into()),
        }
    }

    pub fn shutdown(&self) {
        let endpoint = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.endpoint.take());
        if let Some(endpoint) = endpoint {
            let _ = endpoint.request(BrokerCommand::Shutdown, Duration::from_millis(500));
        }
    }

    fn endpoint_or_start(&self, cancelled: impl Fn() -> bool) -> Result<Endpoint, String> {
        // Drop the mutex guard before pinging.  A dead broker must be removed
        // below, and keeping the temporary guard alive through `if let` would
        // deadlock that recovery path.
        let current = {
            self.state
                .lock()
                .map_err(|_| "privileged broker lock poisoned")?
                .endpoint
                .clone()
        };
        if let Some(endpoint) = current {
            if endpoint.ping().is_ok() {
                return Ok(endpoint);
            }
            self.state
                .lock()
                .map_err(|_| "privileged broker lock poisoned")?
                .endpoint = None;
        }

        let endpoint = start_elevated_broker(&cancelled)?;
        self.state
            .lock()
            .map_err(|_| "privileged broker lock poisoned")?
            .endpoint = Some(endpoint.clone());
        Ok(endpoint)
    }

    fn request_done(
        &self,
        command: BrokerCommand,
        cancelled: impl Fn() -> bool,
        action: &str,
    ) -> Result<(), String> {
        let endpoint = self.endpoint_or_start(cancelled)?;
        match endpoint.request(command, CLIENT_TIMEOUT)? {
            BrokerReply::Done => Ok(()),
            BrokerReply::Error { detail } => Err(detail),
            BrokerReply::Pong | BrokerReply::Partial { .. } => Err(format!(
                "privileged broker returned an invalid {action} reply"
            )),
        }
    }
}

/// The optimistic path.  Developer Mode normally makes this succeed without
/// UAC, so the elevated broker is not created unless Windows actually needs it.
pub fn create_symbolic_link_direct(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(source)?;
    create_symbolic_link_target(source, destination, metadata.is_dir())
}

fn broker_create_symbolic_link(
    source: &Path,
    destination: &Path,
    expected_parent_volume: Option<u32>,
    expected_parent_file_index: Option<u64>,
) -> io::Result<()> {
    let _parent_identity_lease = match (expected_parent_volume, expected_parent_file_index) {
        (None, None) => None,
        (Some(volume), Some(index)) => {
            #[cfg(windows)]
            {
                let identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                    volume, index,
                );
                let parent = destination.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "symbolic-link destination has no parent",
                    )
                })?;
                Some(
                    crate::operations::rename::pin_destination_directory_identity(
                        parent, &identity,
                    )
                    .map_err(|error| {
                        io::Error::other(format!("destination parent identity changed: {error:?}"))
                    })?,
                )
            }
            #[cfg(not(windows))]
            {
                let _ = (volume, index);
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "guarded symbolic-link creation requires Windows",
                ));
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete symbolic-link destination identity",
            ));
        }
    };

    create_symbolic_link_direct(source, destination)
}

/// Create a symbolic link from its already-observed target.  Unlike the
/// clipboard creation path, copying a link must preserve a broken or relative
/// target and therefore must never query metadata for `target`.
pub fn create_symbolic_link_target(
    target: &Path,
    destination: &Path,
    directory: bool,
) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        if directory {
            symlink_dir(target, destination)
        } else {
            symlink_file(target, destination)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (target, destination, directory);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symbolic links are only implemented for Windows",
        ))
    }
}

/// Copy a symbolic-link or junction object without enumerating, opening, or
/// otherwise following its target.  The output is the same reparse kind as
/// the source; other reparse points remain outside this capability.
pub fn copy_link_object_direct(source: &Path, destination: &Path) -> io::Result<LinkObjectKind> {
    let kind = inspect_link_object(source)?;
    let target = std::fs::read_link(source)?;
    create_link_object(kind, &target, destination)?;
    Ok(kind)
}

/// Move a reparse object by creating its exact kind at `destination`, then
/// deleting the held source handle.  If source removal fails, `destination`
/// remains visible and the caller reports a partial move rather than retrying.
pub fn move_link_object_direct(source: &Path, destination: &Path) -> io::Result<LinkMoveResult> {
    move_link_object_direct_with_evidence(source, destination, None, None)
}

/// Move a captured Clipboard reparse object only when the retained no-follow
/// handle still names the exact object and tag observed at Paste time.
pub fn move_link_object_direct_with_evidence(
    source: &Path,
    destination: &Path,
    evidence: Option<&crate::operations::model::ClipboardMoveSourceEvidence>,
    destination_identity: Option<&crate::filesystem_identity::FilesystemIdentity>,
) -> io::Result<LinkMoveResult> {
    #[cfg(windows)]
    {
        let _destination_route = match (evidence, destination_identity) {
            (Some(_), Some(expected)) => Some(lock_link_destination_route(
                destination.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "clipboard destination has no parent"))?,
                expected,
            )?),
            (Some(_), None) => return Err(io::Error::new(io::ErrorKind::InvalidInput, "clipboard destination identity is required")),
            (None, _) => None,
        };
        let source = LinkSource::open_for_move_with_evidence(source, evidence)?;
        let target = std::fs::read_link(source.path())?;
        let kind = source.kind();
        create_link_object(kind, &target, destination)?;
        match source.delete() {
            Ok(()) => Ok(LinkMoveResult::Completed),
            Err(error) => Ok(LinkMoveResult::Partial(format!(
                "destination published; source link deletion failed: {error}"
            ))),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (source, destination, evidence, destination_identity);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "link object moves are only implemented for Windows",
        ))
    }
}

fn create_link_object(kind: LinkObjectKind, target: &Path, destination: &Path) -> io::Result<()> {
    match kind {
        LinkObjectKind::SymbolicLink { directory } => {
            create_symbolic_link_target(target, destination, directory)
        }
        LinkObjectKind::Junction => create_junction_direct(target, destination),
    }
}

/// Read the object tag through a handle opened with `OPEN_REPARSE_POINT`.
/// `metadata` and `FileType::is_symlink` deliberately are not enough here:
/// they erase the symbolic-link/junction distinction on Windows.
pub fn inspect_link_object(source: &Path) -> io::Result<LinkObjectKind> {
    #[cfg(windows)]
    {
        LinkSource::open_for_copy(source).map(|source| source.kind())
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "link objects are only implemented for Windows",
        ))
    }
}

/// Create an NTFS junction without interpolating a filesystem path into a
/// shell command.  Windows exposes no stable `std` junction constructor;
/// PowerShell's `New-Item -ItemType Junction` is used only behind this narrow
/// adapter, with both UTF-16 paths carried as base64 data.
pub fn create_junction_direct(target: &Path, destination: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(target)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a junction target must be an existing folder",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::{ffi::OsStrExt, process::CommandExt};

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let encode_path = |path: &Path| {
            let bytes: Vec<u8> = path
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect();
            encode_base64(&bytes)
        };
        let target = encode_path(target);
        let destination = encode_path(destination);
        let script = format!(
            "$ErrorActionPreference='Stop';$e=[Text.Encoding]::Unicode;$t=$e.GetString([Convert]::FromBase64String('{target}'));$d=$e.GetString([Convert]::FromBase64String('{destination}'));$p=[IO.Path]::GetDirectoryName($d);$n=[IO.Path]::GetFileName($d);$t=[Management.Automation.WildcardPattern]::Escape($t);Push-Location -LiteralPath $p;try{{New-Item -ItemType Junction -Name $n -Target $t -ErrorAction Stop|Out-Null}}finally{{Pop-Location}}"
        );
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "create junction exited {status}: {}",
                String::from_utf8_lossy(&output.stderr).trim(),
                status = output.status
            )))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = destination;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "junctions are only implemented for Windows",
        ))
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((value >> 12) & 0x3f) as usize] as char);
        output.push(
            chunk
                .get(1)
                .map_or('=', |_| TABLE[((value >> 6) & 0x3f) as usize] as char),
        );
        output.push(
            chunk
                .get(2)
                .map_or('=', |_| TABLE[(value & 0x3f) as usize] as char),
        );
    }
    output
}

#[cfg(windows)]
struct LinkSource {
    path: PathBuf,
    file: std::fs::File,
    kind: LinkObjectKind,
}

#[cfg(windows)]
fn lock_link_destination_route(
    destination: &Path,
    expected: &crate::filesystem_identity::FilesystemIdentity,
) -> io::Result<(Vec<std::fs::File>, crate::filesystem_identity::FilesystemIdentity)> {
    use ::windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, GetFileInformationByHandle,
    };
    use std::{
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use ::windows::Win32::Foundation::HANDLE;

    if !destination.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "clipboard destination route must be absolute"));
    }
    let mut route: Vec<_> = destination.ancestors().collect();
    route.reverse();
    let mut handles = Vec::with_capacity(route.len());
    let mut final_identity = None;
    for directory in route {
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0)
            // Keep every checked ancestor stable and reject write/delete access
            // while the absolute-path link creator resolves the same route.
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(directory)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe {
            GetFileInformationByHandle(HANDLE(handle.as_raw_handle() as _), &mut info)
                .map_err(io::Error::other)?;
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "clipboard destination route contains a non-directory or reparse point"));
        }
        final_identity = Some(crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ));
        handles.push(handle);
    }
    let final_identity = final_identity.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "clipboard destination route is empty"))?;
    if &final_identity != expected {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "clipboard destination identity changed before publication"));
    }
    Ok((handles, final_identity))
}

#[cfg(windows)]
impl LinkSource {
    fn open_for_copy(path: &Path) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let kind = link_object_kind_from_file(&file)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            kind,
        })
    }

    fn open_for_move_with_evidence(
        path: &Path,
        evidence: Option<&crate::operations::model::ClipboardMoveSourceEvidence>,
    ) -> io::Result<Self> {
        use ::windows::Win32::Storage::FileSystem::DELETE;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(DELETE.0)
            // Keeping DELETE unshared pins this exact reparse object from
            // creation through deletion; a path reuse cannot delete a later
            // link with the same spelling.
            .share_mode(0x0000_0001)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let kind = link_object_kind_from_file(&file)?;
        if let Some(evidence) = evidence {
            use ::windows::Win32::{
                Foundation::HANDLE,
                Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
            };
            use std::os::windows::io::AsRawHandle;

            if evidence.source != path {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "clipboard source path mismatch"));
            }
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe {
                GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut info)
                    .map_err(io::Error::other)?;
            }
            let identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                info.dwVolumeSerialNumber,
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            );
            let expected_kind = match (evidence.reparse_tag, evidence.source_kind) {
                (Some(0xA000_000C), crate::directory::EntryKind::File) => LinkObjectKind::SymbolicLink { directory: false },
                (Some(0xA000_000C), crate::directory::EntryKind::Folder) => LinkObjectKind::SymbolicLink { directory: true },
                (Some(0xA000_0003), crate::directory::EntryKind::Folder) => LinkObjectKind::Junction,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported captured reparse evidence")),
            };
            if identity != evidence.source_identity || kind != expected_kind {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "clipboard source identity or reparse tag changed"));
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            kind,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn kind(&self) -> LinkObjectKind {
        self.kind
    }

    fn delete(self) -> io::Result<()> {
        use ::windows::Win32::{
            Foundation::{BOOLEAN, HANDLE},
            Storage::FileSystem::{
                FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
            },
        };
        use std::{ffi::c_void, mem::size_of, os::windows::io::AsRawHandle};

        let disposition = FILE_DISPOSITION_INFO {
            DeleteFile: BOOLEAN(1),
        };
        let handle = HANDLE(self.file.as_raw_handle() as *mut _);
        unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfo,
                &disposition as *const _ as *const c_void,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .map_err(io::Error::other)
        }
    }
}

#[cfg(windows)]
fn link_object_kind_from_file(file: &std::fs::File) -> io::Result<LinkObjectKind> {
    use ::windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
            FileAttributeTagInfo, GetFileInformationByHandleEx,
        },
    };
    use std::{ffi::c_void, mem::size_of, os::windows::io::AsRawHandle};

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    let handle = HANDLE(file.as_raw_handle() as *mut _);
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            &mut attributes as *mut _ as *mut c_void,
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
        .map_err(io::Error::other)?;
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a reparse point",
        ));
    }
    match attributes.ReparseTag {
        IO_REPARSE_TAG_SYMLINK => Ok(LinkObjectKind::SymbolicLink {
            directory: attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        }),
        IO_REPARSE_TAG_MOUNT_POINT => Ok(LinkObjectKind::Junction),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported reparse point source",
        )),
    }
}

/// Only these Windows errors enter the explicit elevation flow.  All other
/// errors (missing source, a collision, bad path, …) remain ordinary failures.
pub fn requires_elevation(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5 | 1314))
}

/// Called before GPUI is initialized.  A broker process must never create a
/// window or load an ordinary session.
pub fn run_broker_from_process_args() -> Result<bool, String> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_none_or(|argument| argument != BROKER_ARGUMENT)
    {
        return Ok(false);
    }
    if args.len() != 3 {
        return Err(format!(
            "invalid {BROKER_ARGUMENT} invocation: expected port and capability"
        ));
    }
    let port = args[1]
        .to_string_lossy()
        .parse::<u16>()
        .map_err(|_| "invalid privileged broker port")?;
    let token = args[2].to_string_lossy().into_owned();
    if !valid_token(&token) {
        return Err("invalid privileged broker capability".into());
    }
    run_broker(Endpoint {
        address: SocketAddr::from(([127, 0, 0, 1], port)),
        token,
    })?;
    Ok(true)
}

fn start_elevated_broker(cancelled: &impl Fn() -> bool) -> Result<Endpoint, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("reserve privileged broker endpoint: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("read privileged broker endpoint: {error}"))?
        .port();
    // The port is reserved only to obtain an ephemeral number.  An interloper
    // can at worst cause a denial of service: it cannot answer without the
    // 256-bit capability below.
    drop(listener);
    let endpoint = Endpoint {
        address: SocketAddr::from(([127, 0, 0, 1], port)),
        token: random_token()?,
    };
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve privileged broker executable: {error}"))?;
    let script = format!(
        "Start-Process -FilePath '{}' -Verb RunAs -WindowStyle Hidden -ArgumentList @('{}','{}','{}')",
        escape_powershell_single_quoted(&executable.to_string_lossy()),
        BROKER_ARGUMENT,
        port,
        endpoint.token,
    );
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .spawn()
        .map_err(|error| format!("start privileged broker launcher: {error}"))?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if cancelled() {
            return Err("cancelled while waiting for administrator approval".into());
        }
        if endpoint.ping().is_ok() {
            return Ok(endpoint);
        }
        if Instant::now() >= deadline {
            return Err(
                "administrator approval was declined or the privileged broker did not start".into(),
            );
        }
        std::thread::sleep(CONNECT_RETRY);
    }
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| format!("generate broker capability: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn escape_powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

fn run_broker(endpoint: Endpoint) -> Result<(), String> {
    let listener = TcpListener::bind(endpoint.address)
        .map_err(|error| format!("bind privileged broker: {error}"))?;
    run_broker_on_listener(listener, endpoint)
}

/// Tests keep their already-bound loopback listener so another parallel test
/// cannot claim the ephemeral port in the small hand-off window.
fn run_broker_on_listener(listener: TcpListener, endpoint: Endpoint) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure privileged broker: {error}"))?;
    let mut last_authorized_request = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                // `TcpListener` is loopback-bound, but make that fact explicit
                // before parsing untrusted bytes.
                if !peer.ip().is_loopback() {
                    continue;
                }
                let result = serve_one(&mut stream, &endpoint);
                if matches!(result, Ok(BrokerCommand::Shutdown)) {
                    return Ok(());
                }
                if result.is_ok() {
                    last_authorized_request = Instant::now();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if last_authorized_request.elapsed() >= IDLE_TIMEOUT {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(format!("accept privileged broker request: {error}")),
        }
    }
}

fn serve_one(stream: &mut TcpStream, endpoint: &Endpoint) -> Result<BrokerCommand, String> {
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|error| format!("configure privileged broker request: {error}"))?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|error| format!("configure privileged broker response: {error}"))?;
    let request = read_frame::<BrokerRequest>(stream)?;
    if request.version != PROTOCOL_VERSION || request.token != endpoint.token {
        write_frame(
            stream,
            &BrokerReply::Error {
                detail: "privileged broker capability rejected".into(),
            },
        )?;
        return Err("privileged broker capability rejected".into());
    }
    match &request.command {
        BrokerCommand::Ping => write_frame(stream, &BrokerReply::Pong)?,
        BrokerCommand::Shutdown => write_frame(stream, &BrokerReply::Done)?,
        BrokerCommand::CreateSymbolicLink {
            source,
            destination,
            expected_parent_volume,
            expected_parent_file_index,
        } => {
            let reply = match broker_create_symbolic_link(
                &source.to_path()?,
                &destination.to_path()?,
                *expected_parent_volume,
                *expected_parent_file_index,
            ) {
                Ok(()) => BrokerReply::Done,
                Err(error) => BrokerReply::Error {
                    detail: format!("create symbolic link: {error}"),
                },
            };
            write_frame(stream, &reply)?;
        }
        BrokerCommand::CreateJunction {
            target,
            destination,
        } => {
            let reply = match create_junction_direct(&target.to_path()?, &destination.to_path()?) {
                Ok(()) => BrokerReply::Done,
                Err(error) => BrokerReply::Error {
                    detail: format!("create junction: {error}"),
                },
            };
            write_frame(stream, &reply)?;
        }
        BrokerCommand::CopyRegularFile {
            source,
            destination,
        } => {
            let reply = copy_regular_file_as_broker(&source.to_path()?, &destination.to_path()?);
            write_frame(stream, &reply)?;
        }
        BrokerCommand::MoveRegularFile {
            source,
            destination,
        } => {
            let reply = move_regular_file_as_broker(&source.to_path()?, &destination.to_path()?);
            write_frame(stream, &reply)?;
        }
        BrokerCommand::CopyLinkObject {
            source,
            destination,
        } => {
            let reply = match copy_link_object_direct(&source.to_path()?, &destination.to_path()?) {
                Ok(_) => BrokerReply::Done,
                Err(error) => BrokerReply::Error {
                    detail: format!("copy link object: {error}"),
                },
            };
            write_frame(stream, &reply)?;
        }
        BrokerCommand::MoveLinkObject {
            source,
            destination,
        } => {
            let reply = match move_link_object_direct(&source.to_path()?, &destination.to_path()?) {
                Ok(LinkMoveResult::Completed) => BrokerReply::Done,
                Ok(LinkMoveResult::Partial(detail)) => BrokerReply::Partial { detail },
                Err(error) => BrokerReply::Error {
                    detail: format!("move link object: {error}"),
                },
            };
            write_frame(stream, &reply)?;
        }
        BrokerCommand::MoveLinkObjectWithEvidence {
            source,
            destination,
            source_volume,
            source_file_index,
            reparse_tag,
            source_is_directory,
            destination_volume,
            destination_file_index,
        } => {
            let source = source.to_path()?;
            let evidence = crate::operations::model::ClipboardMoveSourceEvidence {
                source: source.clone(),
                source_identity: crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                    *source_volume, *source_file_index,
                ),
                source_kind: if *source_is_directory {
                    crate::directory::EntryKind::Folder
                } else {
                    crate::directory::EntryKind::File
                },
                reparse_tag: Some(*reparse_tag),
            };
            let destination_identity = crate::filesystem_identity::FilesystemIdentity::from_windows_parts(
                *destination_volume, *destination_file_index,
            );
            let reply = match move_link_object_direct_with_evidence(
                &source, &destination.to_path()?, Some(&evidence), Some(&destination_identity),
            ) {
                Ok(LinkMoveResult::Completed) => BrokerReply::Done,
                Ok(LinkMoveResult::Partial(detail)) => BrokerReply::Partial { detail },
                Err(error) => BrokerReply::Error { detail: format!("move captured link object: {error}") },
            };
            write_frame(stream, &reply)?;
        }
    }
    Ok(request.command)
}

fn copy_regular_file_as_broker(source: &Path, destination: &Path) -> BrokerReply {
    match regular_file_source(source).and_then(|()| {
        let cancelled = AtomicBool::new(false);
        crate::operations::executor::copy_file_no_clobber_without_broker(
            source,
            destination,
            &cancelled,
            &mut |_| {},
        )
        .map_err(|error| format!("copy regular file: {error:?}"))
    }) {
        Ok(()) => BrokerReply::Done,
        Err(detail) => BrokerReply::Error { detail },
    }
}

fn move_regular_file_as_broker(source: &Path, destination: &Path) -> BrokerReply {
    match regular_file_source(source).and_then(|()| {
        let cancelled = AtomicBool::new(false);
        crate::operations::executor::move_file_without_broker(
            source,
            destination,
            &cancelled,
            &mut |_| {},
        )
        .map_err(|error| format!("move regular file: {error:?}"))
    }) {
        Ok(()) => BrokerReply::Done,
        Err(detail) => BrokerReply::Error { detail },
    }
}

/// The broker's loopback capability is deliberately narrower than a shell.
/// Reparse points are never treated as ordinary input, even if a caller knows
/// the random session token.
fn regular_file_source(source: &Path) -> Result<(), String> {
    let metadata = source
        .symlink_metadata()
        .map_err(|error| format!("inspect regular file source: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("broker operations accept only a non-link regular file".into());
    }
    Ok(())
}

impl Endpoint {
    fn ping(&self) -> Result<(), String> {
        match self.request(BrokerCommand::Ping, Duration::from_millis(300))? {
            BrokerReply::Pong => Ok(()),
            _ => Err("privileged broker ping returned an invalid reply".into()),
        }
    }

    fn request(&self, command: BrokerCommand, timeout: Duration) -> Result<BrokerReply, String> {
        self.request_with_timeouts(command, timeout, Some(timeout))
    }

    fn request_until_reply(
        &self,
        command: BrokerCommand,
        connect_write_timeout: Duration,
    ) -> Result<BrokerReply, String> {
        self.request_with_timeouts(command, connect_write_timeout, None)
    }

    fn request_with_timeouts(
        &self,
        command: BrokerCommand,
        connect_write_timeout: Duration,
        response_timeout: Option<Duration>,
    ) -> Result<BrokerReply, String> {
        let mut stream = TcpStream::connect_timeout(&self.address, connect_write_timeout)
            .map_err(|error| format!("connect privileged broker: {error}"))?;
        stream
            .set_read_timeout(response_timeout)
            .map_err(|error| format!("configure privileged broker response: {error}"))?;
        stream
            .set_write_timeout(Some(connect_write_timeout))
            .map_err(|error| format!("configure privileged broker request: {error}"))?;
        write_frame(
            &mut stream,
            &BrokerRequest {
                version: PROTOCOL_VERSION,
                token: self.token.clone(),
                command,
            },
        )?;
        read_frame(&mut stream)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BrokerRequest {
    version: u32,
    token: String,
    command: BrokerCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum BrokerCommand {
    Ping,
    Shutdown,
    CreateSymbolicLink {
        source: WirePath,
        destination: WirePath,
        expected_parent_volume: Option<u32>,
        expected_parent_file_index: Option<u64>,
    },
    CreateJunction {
        target: WirePath,
        destination: WirePath,
    },
    CopyRegularFile {
        source: WirePath,
        destination: WirePath,
    },
    MoveRegularFile {
        source: WirePath,
        destination: WirePath,
    },
    CopyLinkObject {
        source: WirePath,
        destination: WirePath,
    },
    MoveLinkObject {
        source: WirePath,
        destination: WirePath,
    },
    MoveLinkObjectWithEvidence {
        source: WirePath,
        destination: WirePath,
        source_volume: u32,
        source_file_index: u64,
        reparse_tag: u32,
        source_is_directory: bool,
        destination_volume: u32,
        destination_file_index: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum BrokerReply {
    Pong,
    Done,
    Partial { detail: String },
    Error { detail: String },
}

/// Windows paths are UTF-16 units on the wire.  Display strings must never be
/// repurposed as an elevated filesystem target.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WirePath {
    units: Vec<u16>,
}

impl WirePath {
    fn from_path(path: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;

            Self {
                units: path.as_os_str().encode_wide().collect(),
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                units: path.to_string_lossy().encode_utf16().collect(),
            }
        }
    }

    fn to_path(&self) -> Result<PathBuf, String> {
        if self.units.is_empty() || self.units.len() > 32_767 {
            return Err("invalid privileged broker path length".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;

            Ok(PathBuf::from(OsString::from_wide(&self.units)))
        }
        #[cfg(not(windows))]
        {
            String::from_utf16(&self.units)
                .map(PathBuf::from)
                .map_err(|_| "invalid privileged broker path".into())
        }
    }
}

fn read_frame<T: for<'a> Deserialize<'a>>(stream: &mut TcpStream) -> Result<T, String> {
    let mut prefix = [0_u8; 4];
    stream
        .read_exact(&mut prefix)
        .map_err(|error| format!("read privileged broker frame length: {error}"))?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err("invalid privileged broker frame length".into());
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| format!("read privileged broker frame: {error}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode privileged broker frame: {error}"))
}

fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("encode privileged broker frame: {error}"))?;
    let length = u32::try_from(bytes.len()).map_err(|_| "privileged broker frame is too large")?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err("privileged broker frame is too large".into());
    }
    stream
        .write_all(&length.to_le_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .map_err(|error| format!("write privileged broker frame: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_path_preserves_a_non_ascii_windows_path() {
        let path = PathBuf::from(r"C:\テスト\新規ファイル.txt");
        let wire = WirePath::from_path(&path);
        assert_eq!(wire.to_path().unwrap(), path);
    }

    #[test]
    fn rejects_invalid_broker_capabilities() {
        assert!(!valid_token("not-a-token"));
        assert!(!valid_token(&"g".repeat(64)));
        assert!(valid_token(&"0".repeat(64)));
    }

    #[cfg(windows)]
    #[test]
    fn elevation_retry_only_matches_permission_failures() {
        assert!(requires_elevation(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission denied",
        )));
        assert!(requires_elevation(&io::Error::from_raw_os_error(5)));
        assert!(requires_elevation(&io::Error::from_raw_os_error(1314)));
        assert!(!requires_elevation(&io::Error::from_raw_os_error(1)));
        assert!(!requires_elevation(&io::Error::from_raw_os_error(32)));
        assert!(!requires_elevation(&io::Error::from_raw_os_error(183)));
    }

    #[test]
    fn authenticated_broker_session_replies_and_shuts_down() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Endpoint {
            address,
            token: "a".repeat(64),
        };
        let server_endpoint = endpoint.clone();
        let server = std::thread::spawn(move || run_broker_on_listener(listener, server_endpoint));

        let deadline = Instant::now() + Duration::from_secs(2);
        while endpoint.ping().is_err() {
            assert!(Instant::now() < deadline, "broker did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            endpoint
                .request(BrokerCommand::Shutdown, Duration::from_secs(1))
                .unwrap(),
            BrokerReply::Done
        ));
        server.join().unwrap().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn authenticated_broker_reuses_file_and_link_copy_move_adapters() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Endpoint {
            address,
            token: "b".repeat(64),
        };
        let server_endpoint = endpoint.clone();
        let server = std::thread::spawn(move || run_broker_on_listener(listener, server_endpoint));

        let deadline = Instant::now() + Duration::from_secs(2);
        while endpoint.ping().is_err() {
            assert!(Instant::now() < deadline, "broker did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let root = tempfile::tempdir().unwrap();
        let copied_source = root.path().join("copy source [日本語].txt");
        let copied_destination = root.path().join("copied.txt");
        std::fs::write(&copied_source, b"copy contents").unwrap();
        assert!(matches!(
            endpoint
                .request(
                    BrokerCommand::CopyRegularFile {
                        source: WirePath::from_path(&copied_source),
                        destination: WirePath::from_path(&copied_destination),
                    },
                    Duration::from_secs(2),
                )
                .unwrap(),
            BrokerReply::Done
        ));
        assert_eq!(
            std::fs::read(&copied_destination).unwrap(),
            b"copy contents"
        );

        let moved_source = root.path().join("move source.txt");
        let moved_destination = root.path().join("moved.txt");
        std::fs::write(&moved_source, b"move contents").unwrap();
        assert!(matches!(
            endpoint
                .request(
                    BrokerCommand::MoveRegularFile {
                        source: WirePath::from_path(&moved_source),
                        destination: WirePath::from_path(&moved_destination),
                    },
                    Duration::from_secs(2),
                )
                .unwrap(),
            BrokerReply::Done
        ));
        assert!(!moved_source.exists());
        assert_eq!(std::fs::read(&moved_destination).unwrap(), b"move contents");

        let link_target = root.path().join("link target");
        let link_source = root.path().join("link source");
        let link_copy = root.path().join("link copy");
        let link_move = root.path().join("link move");
        std::fs::create_dir(&link_target).unwrap();
        std::fs::write(link_target.join("inside.txt"), b"link contents").unwrap();
        create_junction_direct(&link_target, &link_source).unwrap();
        assert!(matches!(
            endpoint
                .request(
                    BrokerCommand::CopyLinkObject {
                        source: WirePath::from_path(&link_source),
                        destination: WirePath::from_path(&link_copy),
                    },
                    CLIENT_TIMEOUT,
                )
                .unwrap(),
            BrokerReply::Done
        ));
        assert_eq!(
            std::fs::read(link_copy.join("inside.txt")).unwrap(),
            b"link contents"
        );
        assert!(matches!(
            endpoint
                .request(
                    BrokerCommand::MoveLinkObject {
                        source: WirePath::from_path(&link_copy),
                        destination: WirePath::from_path(&link_move),
                    },
                    CLIENT_TIMEOUT,
                )
                .unwrap(),
            BrokerReply::Done
        ));
        assert!(link_copy.symlink_metadata().is_err());
        assert_eq!(
            std::fs::read(link_move.join("inside.txt")).unwrap(),
            b"link contents"
        );

        let stale_parent = root.path().join("captured parent");
        std::fs::create_dir(&stale_parent).unwrap();
        let expected_parent = crate::directory::EntrySourceIdentity::from_path_no_follow(
            &stale_parent,
            crate::directory::EntryKind::Folder,
        )
        .unwrap()
        .filesystem_identity();
        let (expected_parent_volume, expected_parent_file_index) =
            expected_parent.windows_parts().unwrap();
        let displaced_parent = root.path().join("displaced parent");
        std::fs::rename(&stale_parent, &displaced_parent).unwrap();
        std::fs::create_dir(&stale_parent).unwrap();
        let stale_destination = stale_parent.join("must not link");
        let stale_reply = endpoint
            .request(
                BrokerCommand::CreateSymbolicLink {
                    source: WirePath::from_path(&link_source),
                    destination: WirePath::from_path(&stale_destination),
                    expected_parent_volume: Some(expected_parent_volume),
                    expected_parent_file_index: Some(expected_parent_file_index),
                },
                CLIENT_TIMEOUT,
            )
            .unwrap();
        assert!(matches!(stale_reply, BrokerReply::Error { .. }));
        assert!(!stale_destination.exists());

        assert!(matches!(
            endpoint
                .request(BrokerCommand::Shutdown, Duration::from_secs(1))
                .unwrap(),
            BrokerReply::Done
        ));
        server.join().unwrap().unwrap();
    }

    #[test]
    fn terminal_broker_request_waits_for_a_delayed_reply() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Endpoint {
            address,
            token: "c".repeat(64),
        };
        let server_endpoint = endpoint.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_frame::<BrokerRequest>(&mut stream).unwrap();
            assert_eq!(request.token, server_endpoint.token);
            std::thread::sleep(Duration::from_millis(100));
            write_frame(&mut stream, &BrokerReply::Done).unwrap();
        });

        let started = Instant::now();
        let reply = endpoint
            .request_until_reply(BrokerCommand::Ping, Duration::from_secs(1))
            .unwrap();

        assert!(matches!(reply, BrokerReply::Done));
        assert!(started.elapsed() >= Duration::from_millis(100));
        server.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn creates_a_junction_to_a_folder_without_interpolating_its_name() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target [日本語]");
        let junction = root.path().join("junction [日本語]");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside.txt"), b"contents").unwrap();

        create_junction_direct(&target, &junction).unwrap();
        assert!(
            junction
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(junction.join("inside.txt")).unwrap(),
            b"contents"
        );
    }

    #[cfg(windows)]
    #[test]
    fn copies_and_moves_a_junction_without_following_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let source = root.path().join("source");
        let copied = root.path().join("copied");
        let moved = root.path().join("moved");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside.txt"), b"contents").unwrap();
        create_junction_direct(&target, &source).unwrap();

        assert_eq!(
            copy_link_object_direct(&source, &copied).unwrap(),
            LinkObjectKind::Junction
        );
        assert_eq!(
            std::fs::read(copied.join("inside.txt")).unwrap(),
            b"contents"
        );
        assert_eq!(
            move_link_object_direct(&copied, &moved).unwrap(),
            LinkMoveResult::Completed
        );
        assert!(copied.symlink_metadata().is_err());
        assert_eq!(
            std::fs::read(moved.join("inside.txt")).unwrap(),
            b"contents"
        );
        assert_eq!(
            std::fs::read(target.join("inside.txt")).unwrap(),
            b"contents"
        );
    }

    #[cfg(windows)]
    #[test]
    fn copies_and_moves_a_symbolic_link_without_following_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.txt");
        let source = root.path().join("source.txt");
        let copied = root.path().join("copied.txt");
        let moved = root.path().join("moved.txt");
        std::fs::write(&target, b"contents").unwrap();
        if let Err(error) = create_symbolic_link_target(&target, &source, false) {
            if requires_elevation(&error) {
                // Developer Mode is disabled on this machine.  The elevated
                // variant is covered by the broker protocol test instead.
                return;
            }
            panic!("create symbolic-link fixture: {error}");
        }

        assert_eq!(
            copy_link_object_direct(&source, &copied).unwrap(),
            LinkObjectKind::SymbolicLink { directory: false }
        );
        assert_eq!(std::fs::read(&copied).unwrap(), b"contents");
        assert_eq!(
            move_link_object_direct(&copied, &moved).unwrap(),
            LinkMoveResult::Completed
        );
        assert!(copied.symlink_metadata().is_err());
        assert_eq!(std::fs::read(&moved).unwrap(), b"contents");
        assert_eq!(std::fs::read(&target).unwrap(), b"contents");
    }
}
