use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HELPER_SWITCH: &str = "--file-info-shortcut-helper";
const HELPER_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;
const TERMINATE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
static HELPER_DISABLED: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static HELPER_SPAWN_GATE: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

use crate::{directory::EntrySourceIdentity, entry_identity::EntryKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) key: EntryKey,
    pub(crate) path: PathBuf,
    pub(crate) source_identity: EntrySourceIdentity,
    pub(crate) size: u64,
    pub(crate) modified: SystemTime,
    pub(crate) selection_revision: u64,
    pub(crate) location_generation: u64,
    pub(crate) presentation_revision: u64,
    pub(crate) source_view_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Details {
    pub(crate) target: PathBuf,
    pub(crate) target_exists: bool,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) arguments: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Unavailable,
    Loading,
    Ready(Details),
    Failed,
}

#[derive(Clone)]
pub(crate) struct Job {
    pub(crate) ticket: u64,
    pub(crate) target: Target,
    cancelled: Arc<AtomicBool>,
}

pub(crate) struct Completion {
    pub(crate) guard: Option<File>,
    pub(crate) result: Result<Details, ()>,
    pub(crate) disable_helper: bool,
}

/// Shell-owned lifecycle: one running lookup and one replaceable pending target.
#[derive(Default)]
pub(crate) struct Session {
    ticket: u64,
    current: Option<Target>,
    state: State,
    running: Option<(u64, Arc<AtomicBool>)>,
    pending: Option<(u64, Target)>,
    helper_disabled: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::Unavailable
    }
}

impl Session {
    pub(crate) fn state(&self) -> &State {
        &self.state
    }

    pub(crate) fn set_target(&mut self, target: Option<Target>) -> Option<Job> {
        if self.current == target {
            return None;
        }
        self.ticket = self.ticket.wrapping_add(1);
        self.current = target.clone();
        if self.helper_disabled {
            self.pending = None;
            self.state = if target.is_some() {
                State::Failed
            } else {
                State::Unavailable
            };
            return None;
        }
        self.state = if target.is_some() {
            State::Loading
        } else {
            State::Unavailable
        };
        if let Some((_, cancelled)) = &self.running {
            cancelled.store(true, Ordering::Release);
            self.pending = target.map(|target| (self.ticket, target));
            return None;
        }
        target.map(|target| self.start(self.ticket, target))
    }

    pub(crate) fn finish(
        &mut self,
        ticket: u64,
        result: Result<Details, ()>,
        disable_helper: bool,
    ) -> Option<Job> {
        if self
            .running
            .as_ref()
            .is_some_and(|(running, _)| *running == ticket)
        {
            self.running = None;
        }
        if ticket == self.ticket {
            self.state = match result {
                Ok(details) => State::Ready(details),
                Err(()) => State::Failed,
            };
        }
        if disable_helper {
            self.helper_disabled = true;
            self.pending = None;
            self.state = if self.current.is_some() {
                State::Failed
            } else {
                State::Unavailable
            };
        }
        if self.helper_disabled {
            return None;
        }
        self.pending
            .take()
            .map(|(ticket, target)| self.start(ticket, target))
    }

    fn start(&mut self, ticket: u64, target: Target) -> Job {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.running = Some((ticket, cancelled.clone()));
        Job {
            ticket,
            target,
            cancelled,
        }
    }
}

pub(crate) fn row_job(ticket: u64, target: Target) -> Job {
    Job {
        ticket,
        target,
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

pub(crate) fn is_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
}

/// Runs from a detached executor task. Potentially blocking work is isolated in
/// a supervised disposable process; the caller holds the returned guard to commit.
pub(crate) fn read(job: &Job) -> Completion {
    #[cfg(windows)]
    {
        read_supervised(job)
    }
    #[cfg(not(windows))]
    {
        let _ = job;
        failed()
    }
}

/// Early process dispatch in main; this path never initializes GPUI or the broker.
pub(crate) fn run_helper_from_process_args() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(HELPER_SWITCH)) {
        return false;
    }
    #[cfg(windows)]
    helper_main();
    std::process::exit(1)
}

#[cfg(windows)]
struct UnreapedChild {
    child: Child,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

#[cfg(windows)]
static UNREAPED_CHILD: OnceLock<std::sync::Mutex<Option<UnreapedChild>>> = OnceLock::new();

#[cfg(windows)]
fn read_supervised(job: &Job) -> Completion {
    use std::os::windows::io::FromRawHandle;
    let spawn_gate = HELPER_SPAWN_GATE.get_or_init(|| std::sync::Mutex::new(()));
    let spawn_guard = spawn_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if HELPER_DISABLED.load(Ordering::Acquire) {
        return failed_disabled();
    }
    let Some(identity) = job.target.source_identity.helper_transport_parts() else {
        return failed();
    };
    let Some(request) = encode_request(&job.target, identity) else {
        return failed();
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return failed(),
    };
    let mut child = match Command::new(executable)
        .arg(HELPER_SWITCH)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return failed(),
    };
    drop(spawn_guard);
    let Some(child_stdin) = child.stdin.take() else {
        return terminate_child(child, None, None, None);
    };
    let Some(stdout) = child.stdout.take() else {
        return terminate_child(child, None, None, None);
    };

    let (ack_tx, ack_rx) = mpsc::sync_channel::<bool>(1);
    let (write_tx, write_rx) = mpsc::sync_channel::<bool>(1);
    let writer = match thread::Builder::new()
        .name("file-info-shortcut-request".into())
        .spawn(move || {
            let mut pipe = child_stdin;
            let written = pipe.write_all(&request).is_ok() && pipe.flush().is_ok();
            let _ = write_tx.send(written);
            if written && ack_rx.recv().unwrap_or(false) {
                let _ = pipe.write_all(&[1]);
                let _ = pipe.flush();
            }
        }) {
        Ok(thread) => thread,
        Err(_) => return terminate_child(child, None, None, None),
    };
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let reader = match thread::Builder::new()
        .name("file-info-shortcut-response".into())
        .spawn(move || {
            let mut pipe = stdout;
            let _ = response_tx.send(read_response(&mut pipe));
        }) {
        Ok(thread) => thread,
        Err(_) => return terminate_child(child, Some(ack_tx), None, Some(writer)),
    };

    let deadline = Instant::now() + HELPER_TIMEOUT;
    let request_written = loop {
        if job.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return terminate_child(child, Some(ack_tx), Some(reader), Some(writer));
        }
        if let Ok(written) = write_rx.recv_timeout(Duration::from_millis(10)) {
            if !written {
                return terminate_child(child, Some(ack_tx), Some(reader), Some(writer));
            }
            break true;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                let _ = ack_tx.send(false);
                let _ = reader.join();
                let _ = writer.join();
                return failed();
            }
            Ok(None) => {}
            Err(_) => return terminate_child(child, Some(ack_tx), Some(reader), Some(writer)),
        }
    };
    debug_assert!(request_written);

    let response = loop {
        if job.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return terminate_child(child, Some(ack_tx), Some(reader), Some(writer));
        }
        if let Ok(response) = response_rx.recv_timeout(Duration::from_millis(10)) {
            break response;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                let _ = ack_tx.send(false);
                let _ = reader.join();
                let _ = writer.join();
                return failed();
            }
            Ok(None) => {}
            Err(_) => return terminate_child(child, Some(ack_tx), Some(reader), Some(writer)),
        }
    };
    let Some((details, raw_guard)) = response else {
        return terminate_child(child, Some(ack_tx), Some(reader), Some(writer));
    };
    if raw_guard == 0 || job.cancelled.load(Ordering::Acquire) {
        return terminate_child(child, Some(ack_tx), Some(reader), Some(writer));
    }
    let Some(handle) = duplicate_child_handle(child.id(), raw_guard) else {
        return terminate_child(child, Some(ack_tx), Some(reader), Some(writer));
    };
    let guard = unsafe { File::from_raw_handle(handle as _) };
    if ack_tx.send(true).is_err() {
        drop(guard);
        return terminate_child(child, None, Some(reader), Some(writer));
    }
    loop {
        if job.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            drop(guard);
            return terminate_child(child, None, Some(reader), None);
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let _ = writer.join();
                let _ = reader.join();
                return Completion {
                    guard: Some(guard),
                    result: Ok(details),
                    disable_helper: false,
                };
            }
            Ok(Some(_)) => {
                drop(guard);
                let _ = writer.join();
                let _ = reader.join();
                return failed();
            }
            Err(_) => {
                drop(guard);
                return terminate_child(child, None, Some(reader), None);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(windows)]
fn terminate_child(
    mut child: Child,
    ack: Option<mpsc::SyncSender<bool>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
) -> Completion {
    if let Some(ack) = ack {
        let _ = ack.send(false);
    }
    let _ = child.kill();
    let deadline = Instant::now() + TERMINATE_CONFIRM_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
                if let Some(writer) = writer {
                    let _ = writer.join();
                }
                return failed();
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let spawn_gate = HELPER_SPAWN_GATE.get_or_init(|| std::sync::Mutex::new(()));
                let _spawn_guard = spawn_gate
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                HELPER_DISABLED.store(true, Ordering::Release);
                let retained = Arc::new(std::sync::Mutex::new(Some(UnreapedChild {
                    child,
                    reader,
                    writer,
                })));
                let reaper_state = retained.clone();
                let reaper = thread::Builder::new()
                    .name("file-info-shortcut-reaper".into())
                    .spawn(move || {
                        let unreaped = reaper_state.lock().ok().and_then(|mut value| value.take());
                        if let Some(mut unreaped) = unreaped {
                            let _ = unreaped.child.wait();
                            if let Some(reader) = unreaped.reader.take() {
                                let _ = reader.join();
                            }
                            if let Some(writer) = unreaped.writer.take() {
                                let _ = writer.join();
                            }
                        }
                    });
                if reaper.is_err() {
                    // No helper may be started again. Retain the sole unreaped
                    // process handle if the OS cannot provide a reaper thread.
                    let slot = UNREAPED_CHILD.get_or_init(|| std::sync::Mutex::new(None));
                    if let Ok(mut slot) = slot.lock() {
                        *slot = retained.lock().ok().and_then(|mut value| value.take());
                    }
                }
                return failed_disabled();
            }
        }
    }
}

#[cfg(windows)]
fn duplicate_child_handle(pid: u32, raw: u64) -> Option<usize> {
    use windows::Win32::{
        Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE},
        System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE},
    };
    let process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, pid) }.ok()?;
    let mut duplicate = HANDLE::default();
    let ok = unsafe {
        DuplicateHandle(
            process,
            HANDLE(raw as _),
            GetCurrentProcess(),
            &mut duplicate,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    ok.ok()?;
    Some(duplicate.0 as usize)
}

#[cfg(windows)]
fn write_request(
    writer: &mut impl Write,
    target: &Target,
    identity: (u32, u64),
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = target.path.as_os_str().encode_wide().collect();
    if path.is_empty() || path.len() > 32_000 {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let modified = target
        .modified
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?;
    writer.write_all(&(path.len() as u32).to_le_bytes())?;
    for unit in path {
        writer.write_all(&unit.to_le_bytes())?;
    }
    writer.write_all(&target.size.to_le_bytes())?;
    writer.write_all(&modified.as_secs().to_le_bytes())?;
    writer.write_all(&modified.subsec_nanos().to_le_bytes())?;
    writer.write_all(&identity.0.to_le_bytes())?;
    writer.write_all(&identity.1.to_le_bytes())?;
    writer.flush()
}

#[cfg(windows)]
fn encode_request(target: &Target, identity: (u32, u64)) -> Option<Vec<u8>> {
    let mut request = Vec::with_capacity(64 * 1024);
    write_request(&mut request, target, identity).ok()?;
    (request.len() <= MAX_PROTOCOL_BYTES).then_some(request)
}

#[cfg(windows)]
fn read_response(reader: &mut impl Read) -> Option<(Details, u64)> {
    let mut status = [0; 1];
    reader.read_exact(&mut status).ok()?;
    if status[0] != 1 {
        return None;
    }
    let target = read_path(reader)?;
    let mut exists = [0; 1];
    reader.read_exact(&mut exists).ok()?;
    let working_directory = read_optional_path(reader)?;
    let arguments = read_optional_string(reader)?;
    let mut raw = [0; 8];
    reader.read_exact(&mut raw).ok()?;
    Some((
        Details {
            target,
            target_exists: exists[0] != 0,
            working_directory,
            arguments,
        },
        u64::from_le_bytes(raw),
    ))
}

#[cfg(windows)]
fn read_path(reader: &mut impl Read) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut n = [0; 4];
    reader.read_exact(&mut n).ok()?;
    let n = u32::from_le_bytes(n) as usize;
    if n == 0 || n > MAX_PROTOCOL_BYTES / 2 {
        return None;
    }
    let mut bytes = vec![0; n.checked_mul(2)?];
    reader.read_exact(&mut bytes).ok()?;
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Some(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
}

#[cfg(windows)]
fn read_optional_path(reader: &mut impl Read) -> Option<Option<PathBuf>> {
    let mut flag = [0; 1];
    reader.read_exact(&mut flag).ok()?;
    (flag[0] == 0)
        .then_some(None)
        .or_else(|| read_path(reader).map(Some))
}

#[cfg(windows)]
fn read_optional_string(reader: &mut impl Read) -> Option<Option<String>> {
    let mut flag = [0; 1];
    reader.read_exact(&mut flag).ok()?;
    if flag[0] == 0 {
        return Some(None);
    }
    let mut n = [0; 4];
    reader.read_exact(&mut n).ok()?;
    let n = u32::from_le_bytes(n) as usize;
    if n > MAX_PROTOCOL_BYTES {
        return None;
    }
    let mut bytes = vec![0; n];
    reader.read_exact(&mut bytes).ok()?;
    String::from_utf8(bytes).ok().map(Some)
}

#[cfg(windows)]
fn helper_main() {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    let result = (|| {
        let mut input = std::io::stdin().lock();
        let mut count = [0; 4];
        input.read_exact(&mut count).ok()?;
        let count = u32::from_le_bytes(count) as usize;
        if count == 0 || count > 32_000 {
            return None;
        }
        let mut bytes = vec![0; count.checked_mul(2)?];
        input.read_exact(&mut bytes).ok()?;
        let wide = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<_>>();
        let path = PathBuf::from(std::ffi::OsString::from_wide(&wide));
        let mut fixed = [0; 32];
        input.read_exact(&mut fixed).ok()?;
        let size = u64::from_le_bytes(fixed[0..8].try_into().ok()?);
        let seconds = u64::from_le_bytes(fixed[8..16].try_into().ok()?);
        let nanos = u32::from_le_bytes(fixed[16..20].try_into().ok()?);
        if nanos >= 1_000_000_000 {
            return None;
        }
        let volume = u32::from_le_bytes(fixed[20..24].try_into().ok()?);
        let index = u64::from_le_bytes(fixed[24..32].try_into().ok()?);
        let modified = UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))?;
        let target = Target {
            key: EntryKey::path(path.clone(), crate::directory::EntryKind::File),
            path,
            source_identity: EntrySourceIdentity::from_helper_transport_parts(
                crate::directory::EntryKind::File,
                volume,
                index,
            ),
            size,
            modified,
            selection_revision: 0,
            location_generation: 0,
            presentation_revision: 0,
            source_view_revision: 0,
        };
        let file = open_read_target(&target).ok()?;
        let before = signature(&file, &target)?;
        let parsed = resolve_on_sta(&target.path)?;
        if signature(&file, &target) != Some(before) {
            return None;
        }
        let target_exists = parsed.target.exists();
        if signature(&file, &target) != Some(before) {
            return None;
        }
        let guard = open_commit_guard(&target).ok()?;
        if signature(&guard, &target) != Some(before) {
            return None;
        }
        Some((
            Details {
                target: parsed.target,
                target_exists,
                working_directory: parsed.working_directory,
                arguments: parsed.arguments,
            },
            guard,
        ))
    })();
    let mut stdout = std::io::stdout().lock();
    match result {
        Some((details, guard)) => {
            if write_helper_response(&mut stdout, &details, guard.as_raw_handle() as usize as u64)
                .is_err()
            {
                std::process::exit(2);
            }
            let mut ack = [0; 1];
            if std::io::stdin().lock().read_exact(&mut ack).is_err() || ack[0] != 1 {
                std::process::exit(2);
            }
            drop(guard);
            std::process::exit(0);
        }
        None => {
            let _ = stdout.write_all(&[0]);
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn write_helper_response(
    writer: &mut impl Write,
    details: &Details,
    raw_guard: u64,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    fn write_path(writer: &mut impl Write, path: &Path) -> std::io::Result<()> {
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.is_empty() || wide.len() > MAX_PROTOCOL_BYTES / 2 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        writer.write_all(&(wide.len() as u32).to_le_bytes())?;
        for unit in wide {
            writer.write_all(&unit.to_le_bytes())?;
        }
        Ok(())
    }
    writer.write_all(&[1])?;
    write_path(writer, &details.target)?;
    writer.write_all(&[u8::from(details.target_exists)])?;
    if let Some(path) = &details.working_directory {
        writer.write_all(&[1])?;
        write_path(writer, path)?;
    } else {
        writer.write_all(&[0])?;
    }
    if let Some(args) = &details.arguments {
        let bytes = args.as_bytes();
        if bytes.len() > MAX_PROTOCOL_BYTES {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        writer.write_all(&[1])?;
        writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
        writer.write_all(bytes)?;
    } else {
        writer.write_all(&[0])?;
    }
    writer.write_all(&raw_guard.to_le_bytes())?;
    writer.flush()
}

struct ParsedDetails {
    target: PathBuf,
    working_directory: Option<PathBuf>,
    arguments: Option<String>,
}

#[cfg(windows)]
fn resolve_on_sta(path: &Path) -> Option<ParsedDetails> {
    use std::{
        os::windows::ffi::{OsStrExt, OsStringExt},
        ptr,
    };
    use windows::{
        Win32::{
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                IPersistFile, STGM_READ,
            },
            UI::Shell::{IShellLinkW, ShellLink},
        },
        core::{Interface as _, PCWSTR},
    };

    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if initialized.is_err() {
        // RPC_E_CHANGED_MODE も含め、STA を保証できない thread は使わない。
        return None;
    }
    let _com = ComUninitialize;
    let parsed = (|| {
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe { persist.Load(PCWSTR(wide.as_ptr()), STGM_READ) }.ok()?;

        let mut target = vec![0u16; 32768];
        unsafe { link.GetPath(&mut target, ptr::null_mut(), 0) }.ok()?;
        let target_len = target.iter().position(|unit| *unit == 0)?;
        if target_len == 0 {
            return None;
        }
        let target = PathBuf::from(std::ffi::OsString::from_wide(&target[..target_len]));

        let mut working = vec![0u16; 32768];
        let working_directory = unsafe { link.GetWorkingDirectory(&mut working) }
            .ok()
            .and_then(|_| working.iter().position(|unit| *unit == 0))
            .filter(|length| *length > 0)
            .map(|length| PathBuf::from(std::ffi::OsString::from_wide(&working[..length])));
        let mut args = vec![0u16; 32768];
        let arguments = unsafe { link.GetArguments(&mut args) }
            .ok()
            .and_then(|_| args.iter().position(|unit| *unit == 0))
            .filter(|length| *length > 0)
            .map(|length| String::from_utf16_lossy(&args[..length]))
            .filter(|value| !value.trim().is_empty());
        Some(ParsedDetails {
            target,
            working_directory,
            arguments,
        })
    })();
    // parsed 内の interface はこの STA thread 上で先に drop される。
    parsed
}

#[cfg(windows)]
struct ComUninitialize;

#[cfg(windows)]
impl Drop for ComUninitialize {
    fn drop(&mut self) {
        use windows::Win32::System::Com::CoUninitialize;
        unsafe { CoUninitialize() };
    }
}

#[cfg(not(windows))]
fn resolve_on_sta(_path: &Path) -> Option<ParsedDetails> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Signature(u64, u64, SystemTime, i64);

fn signature(file: &File, target: &Target) -> Option<Signature> {
    let metadata = file.metadata().ok()?;
    let change_time = change_time(file)?;
    (metadata.is_file()
        && metadata.len() == target.size
        && metadata.modified().ok() == Some(target.modified)
        && target.source_identity.matches_file(file))
    .then_some(Signature(
        target.size,
        metadata.len(),
        target.modified,
        change_time,
    ))
}

#[cfg(windows)]
fn change_time(file: &File) -> Option<i64> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx},
    };
    let mut basic = FILE_BASIC_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle() as _),
            FileBasicInfo,
            &mut basic as *mut _ as *mut _,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
        .ok()?;
    }
    Some(basic.ChangeTime)
}

#[cfg(not(windows))]
fn change_time(_file: &File) -> Option<i64> {
    None
}

#[cfg(windows)]
fn open_read_target(target: &Target) -> Result<File, ()> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(0x0020_0000)
        .open(&target.path)
        .map_err(|_| ())
}

#[cfg(windows)]
fn open_commit_guard(target: &Target) -> Result<File, ()> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1)
        .custom_flags(0x0020_0000)
        .open(&target.path)
        .map_err(|_| ())
}

#[cfg(not(windows))]
fn open_read_target(_target: &Target) -> Result<File, ()> {
    Err(())
}
#[cfg(not(windows))]
fn open_commit_guard(_target: &Target) -> Result<File, ()> {
    Err(())
}

fn failed() -> Completion {
    Completion {
        guard: None,
        result: Err(()),
        disable_helper: false,
    }
}

fn failed_disabled() -> Completion {
    Completion {
        guard: None,
        result: Err(()),
        disable_helper: true,
    }
}
