//! Everything IPC の provider owner。
//!
//! `DirectorySession` と `FilerShell` は request identity の authority のままにし、
//! ここは provider の epoch、共有 slot、使い捨て cache、typed outcome だけを持つ。

use crate::{
    directory::{EntrySourceIdentity, FolderSizeFact},
    entry_identity::EntryKey,
};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProviderRevision(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FolderSizeCacheGeneration(u64);

impl ProviderRevision {
    pub fn current() -> Self {
        provider().revision()
    }
}

#[cfg(test)]
pub(crate) fn occupy_provider_slot_for_test() -> async_lock::SemaphoreGuardArc {
    provider()
        .slots
        .try_acquire_arc()
        .expect("test provider slot available")
}

#[cfg(test)]
pub(crate) fn provider_has_no_free_slot_for_test() -> bool {
    match provider().slots.try_acquire_arc() {
        Some(slot) => {
            drop(slot);
            false
        }
        None => true,
    }
}

pub fn current_folder_size_cache_generation() -> FolderSizeCacheGeneration {
    provider().folder_size_cache_generation()
}

pub fn invalidate_folder_size_cache_under(path: &Path) -> FolderSizeCacheGeneration {
    provider().invalidate_folder_size_cache(path)
}

#[derive(Clone)]
pub struct FolderSizeFreshness(Arc<std::sync::atomic::AtomicBool>);

impl std::fmt::Debug for FolderSizeFreshness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("FolderSizeFreshness")
            .field(&self.is_current())
            .finish()
    }
}

impl PartialEq for FolderSizeFreshness {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for FolderSizeFreshness {}

impl FolderSizeFreshness {
    pub(crate) fn new() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }

    pub fn is_current(&self) -> bool {
        !self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct FolderSizeCacheLease {
    registrations: Mutex<Vec<(String, Weak<std::sync::atomic::AtomicBool>)>>,
}

impl Drop for FolderSizeCacheLease {
    fn drop(&mut self) {
        let registrations = self
            .registrations
            .get_mut()
            .expect("Everything lease poisoned");
        let mut active = provider()
            .active_probes
            .lock()
            .expect("Everything active probes poisoned");
        for (path, registered) in registrations.drain(..) {
            if let Some(probes) = active.get_mut(&path) {
                probes.retain(|probe| !probe.ptr_eq(&registered));
                if probes.is_empty() {
                    active.remove(&path);
                }
            }
        }
    }
}

impl FolderSizeCacheLease {
    pub fn register(&self, path: &Path) -> FolderSizeFreshness {
        let freshness = FolderSizeFreshness::new();
        let normalized_path = normalize_path(path);
        let weak = Arc::downgrade(&freshness.0);
        self.registrations
            .lock()
            .expect("Everything lease poisoned")
            .push((normalized_path.clone(), weak.clone()));
        let mut active = provider()
            .active_probes
            .lock()
            .expect("Everything active probes poisoned");
        let probes = active.entry(normalized_path).or_default();
        probes.retain(|probe| probe.strong_count() > 0);
        probes.push(weak);
        freshness
    }
}

pub fn begin_folder_size_cache_lease() -> (FolderSizeCacheGeneration, Arc<FolderSizeCacheLease>) {
    (
        current_folder_size_cache_generation(),
        Arc::new(FolderSizeCacheLease {
            registrations: Mutex::new(Vec::new()),
        }),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSizeProbe {
    pub key: EntryKey,
    pub path: PathBuf,
    pub source_identity: EntrySourceIdentity,
    pub cache_generation: FolderSizeCacheGeneration,
    pub freshness: FolderSizeFreshness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSizeResult {
    pub key: EntryKey,
    pub path: PathBuf,
    pub fact: FolderSizeFact,
    pub revision: ProviderRevision,
    pub cache_generation: FolderSizeCacheGeneration,
    pub freshness: FolderSizeFreshness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<u64>,
    pub revision: ProviderRevision,
}

pub const SEARCH_MAX_RESULTS: u32 = 50;
pub const FOLDER_SIZE_DEADLINE: Duration = Duration::from_millis(1_500);
const PROVIDER_SLOTS: usize = 2;
const FOLDER_CACHE_CAPACITY: usize = 128;
const HELPER_SWITCH: &str = "--everything-ipc-helper";
#[cfg(feature = "everything-private-ipc-test")]
const PRIVATE_IPC_TEST_INSTANCE_ENV: &str = "GPUI_FILER_EVERYTHING_PRIVATE_IPC_TEST_INSTANCE";
#[cfg(feature = "everything-private-ipc-test")]
const PRIVATE_IPC_TEST_HELPER_PID_ENV: &str = "GPUI_FILER_EVERYTHING_PRIVATE_IPC_TEST_HELPER_PID";
#[cfg(feature = "everything-private-ipc-test")]
const PRIVATE_IPC_TEST_PREFLIGHT_ENV: &str = "GPUI_FILER_EVERYTHING_PRIVATE_IPC_TEST_PREFLIGHT";
#[cfg(feature = "everything-private-ipc-test")]
const PRIVATE_IPC_TEST_PREFLIGHT_RECEIPT: &str = "gpui-filer-private-ipc-test-v1";
const HELPER_PROTOCOL_VERSION: u16 = 1;
const HELPER_MAX_FRAME: usize = 16 * 1024 * 1024;
const HELPER_MAX_PATH_UNITS: usize = 32_000;
const HELPER_MAX_FOLDER_PATHS: usize = 512;
const HELPER_MAX_QUERY_BYTES: usize = 4_096;
const TERMINATE_CONFIRM_TIMEOUT: Duration = Duration::from_millis(500);
static NEXT_HELPER_REQUEST: AtomicU64 = AtomicU64::new(1);
static HELPER_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderTerminal {
    Completed,
    Cancelled,
    Unavailable,
    Rejected,
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerError {
    Cancelled,
    Timeout,
    Unavailable,
    Rejected,
}

enum HelperDecodeError {
    Provider,
    Protocol,
}

impl From<String> for HelperDecodeError {
    fn from(_: String) -> Self {
        Self::Protocol
    }
}

impl From<&str> for HelperDecodeError {
    fn from(_: &str) -> Self {
        Self::Protocol
    }
}

impl From<String> for WorkerError {
    fn from(_: String) -> Self {
        Self::Unavailable
    }
}

impl From<&str> for WorkerError {
    fn from(_: &str) -> Self {
        Self::Unavailable
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSizeOutcome {
    pub revision: ProviderRevision,
    pub cache_generation: FolderSizeCacheGeneration,
    pub results: Vec<FolderSizeResult>,
    pub retry_invalidated: bool,
    pub terminal: ProviderTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOutcome {
    pub revision: ProviderRevision,
    pub results: Vec<SearchResult>,
    pub terminal: ProviderTerminal,
    pub error: Option<String>,
}

#[derive(Clone)]
struct CachedFolderSize {
    path: String,
    source_identity: EntrySourceIdentity,
    revision: ProviderRevision,
    fact: FolderSizeFact,
    // IPC開始順。workerの復帰・呼出側の再開順が逆転しても新しい同一path値を守る。
    request_order: u64,
}

struct Provider {
    revision: AtomicU64,
    cache_generation: AtomicU64,
    next_folder_size_request_order: AtomicU64,
    slots: Arc<async_lock::Semaphore>,
    cache: Mutex<VecDeque<CachedFolderSize>>,
    active_probes: Mutex<HashMap<String, Vec<Weak<std::sync::atomic::AtomicBool>>>>,
}

fn provider() -> &'static Provider {
    static PROVIDER: OnceLock<Provider> = OnceLock::new();
    PROVIDER.get_or_init(|| Provider {
        revision: AtomicU64::new(1),
        cache_generation: AtomicU64::new(1),
        next_folder_size_request_order: AtomicU64::new(1),
        slots: Arc::new(async_lock::Semaphore::new(PROVIDER_SLOTS)),
        cache: Mutex::new(VecDeque::with_capacity(FOLDER_CACHE_CAPACITY)),
        active_probes: Mutex::new(HashMap::new()),
    })
}

impl Provider {
    fn revision(&self) -> ProviderRevision {
        ProviderRevision(self.revision.load(Ordering::Acquire))
    }

    /// 同じ古い epoch の失敗は一回だけ provider epoch を進める。
    fn invalidate(&self, expected: ProviderRevision) -> ProviderRevision {
        let mut cache = self.cache.lock().expect("Everything cache poisoned");
        if self.revision() == expected {
            let _ = self.revision.compare_exchange(
                expected.0,
                expected.0.wrapping_add(1).max(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            cache.clear();
        }
        self.revision()
    }

    /// DirectorySession の F5/watch invalidation から呼ばれ、旧 worker の再投入を防ぐ。
    fn invalidate_folder_size_cache(&self, path: &Path) -> FolderSizeCacheGeneration {
        let mut cache = self.cache.lock().expect("Everything cache poisoned");
        let generation = self
            .cache_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let normalized_path = normalize_path(path);
        cache.retain(|entry| !paths_intersect(&entry.path, &normalized_path));
        self.active_probes
            .lock()
            .expect("Everything active probes poisoned")
            .retain(|probe_path, probes| {
                if paths_intersect(probe_path, &normalized_path) {
                    for probe in probes.iter().filter_map(Weak::upgrade) {
                        probe.store(true, Ordering::Release);
                    }
                }
                probes.retain(|probe| probe.strong_count() > 0);
                !probes.is_empty()
            });
        FolderSizeCacheGeneration(generation)
    }

    fn folder_size_cache_generation(&self) -> FolderSizeCacheGeneration {
        FolderSizeCacheGeneration(self.cache_generation.load(Ordering::Acquire))
    }

    fn cached(
        &self,
        path: &Path,
        identity: &EntrySourceIdentity,
        revision: ProviderRevision,
        allow: bool,
    ) -> Option<FolderSizeFact> {
        if !allow {
            return None;
        }
        let key = normalize_path(path);
        let mut cache = self.cache.lock().expect("Everything cache poisoned");
        let index = cache.iter().position(|entry| {
            entry.path == key && entry.source_identity == *identity && entry.revision == revision
        })?;
        let entry = cache.remove(index)?;
        let fact = entry.fact;
        cache.push_back(entry);
        Some(fact)
    }

    fn insert(
        &self,
        path: &Path,
        fact: FolderSizeFact,
        identity: EntrySourceIdentity,
        revision: ProviderRevision,
        freshness: &FolderSizeFreshness,
        request_order: u64,
        cancelled: &std::sync::atomic::AtomicBool,
    ) {
        let mut cache = self.cache.lock().expect("Everything cache poisoned");
        // Invalidation and insertion share this lock.  古いrequestやcancel済みrequestの完了を
        // newer な同一path cache fact へ上書きさせない。
        if cancelled.load(Ordering::Acquire)
            || self.revision() != revision
            || !freshness.is_current()
        {
            return;
        }
        let normalized_path = normalize_path(path);
        if cache
            .iter()
            .any(|entry| entry.path == normalized_path && entry.request_order > request_order)
        {
            return;
        }
        cache.retain(|entry| entry.path != normalized_path);
        cache.push_back(CachedFolderSize {
            path: normalized_path,
            source_identity: identity,
            revision,
            fact,
            request_order,
        });
        while cache.len() > FOLDER_CACHE_CAPACITY {
            cache.pop_front();
        }
    }
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn paths_intersect(left: &str, right: &str) -> bool {
    path_is_within(left, right) || path_is_within(right, left)
}

fn path_is_within(path: &str, root: &str) -> bool {
    root.is_empty()
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| root.ends_with('\\') || suffix.starts_with('\\'))
}

async fn acquire_slot(
    executor: &gpui::BackgroundExecutor,
    cancelled: &std::sync::atomic::AtomicBool,
    deadline: Instant,
) -> Result<async_lock::SemaphoreGuardArc, ProviderTerminal> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(ProviderTerminal::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ProviderTerminal::Timeout);
        }
        if let Some(slot) = provider().slots.try_acquire_arc() {
            return Ok(slot);
        }
        executor.timer(Duration::from_millis(5)).await;
    }
}

fn resolve_worker_poll<T>(
    message: Option<Result<T, WorkerError>>,
    disconnected: bool,
    cancelled: bool,
    deadline_elapsed: bool,
) -> Option<Result<T, ProviderTerminal>> {
    if let Some(message) = message {
        return Some(message.map_err(|error| match error {
            WorkerError::Cancelled => ProviderTerminal::Cancelled,
            WorkerError::Timeout => ProviderTerminal::Timeout,
            WorkerError::Unavailable => ProviderTerminal::Unavailable,
            WorkerError::Rejected => ProviderTerminal::Rejected,
        }));
    }
    if disconnected {
        return Some(Err(ProviderTerminal::Unavailable));
    }
    if cancelled {
        return Some(Err(ProviderTerminal::Cancelled));
    }
    deadline_elapsed.then_some(Err(ProviderTerminal::Timeout))
}

async fn wait_for_worker<T: Send + 'static>(
    executor: &gpui::BackgroundExecutor,
    receiver: mpsc::Receiver<Result<T, WorkerError>>,
    cancelled: &std::sync::atomic::AtomicBool,
    deadline: Instant,
    supervisor_cancelled: &std::sync::atomic::AtomicBool,
) -> Result<T, ProviderTerminal> {
    loop {
        let (message, disconnected) = match receiver.try_recv() {
            Ok(message) => (Some(message), false),
            Err(mpsc::TryRecvError::Disconnected) => (None, true),
            Err(mpsc::TryRecvError::Empty) => (None, false),
        };
        if let Some(result) = resolve_worker_poll(
            message,
            disconnected,
            cancelled.load(Ordering::Acquire),
            Instant::now() >= deadline,
        ) {
            if matches!(
                result,
                Err(ProviderTerminal::Cancelled | ProviderTerminal::Timeout)
            ) {
                supervisor_cancelled.store(true, Ordering::Release);
            }
            return result;
        }
        executor.timer(Duration::from_millis(2)).await;
    }
}

#[derive(Clone)]
enum HelperRequest {
    FolderSizes(Vec<PathBuf>),
    Search(String),
}

enum HelperReply {
    FolderSizes(Vec<Option<u64>>),
    Search(Vec<SearchResult>),
}

fn next_helper_id() -> u64 {
    NEXT_HELPER_REQUEST.fetch_add(1, Ordering::Relaxed).max(1)
}

#[cfg(windows)]
fn encode_helper_request(
    request: &HelperRequest,
    id: u64,
    revision: ProviderRevision,
) -> Option<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"EIPC");
    bytes.extend_from_slice(&HELPER_PROTOCOL_VERSION.to_le_bytes());
    bytes.push(match request {
        HelperRequest::FolderSizes(_) => 1,
        HelperRequest::Search(_) => 2,
    });
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&revision.0.to_le_bytes());
    match request {
        HelperRequest::FolderSizes(paths) => {
            if paths.is_empty() || paths.len() > HELPER_MAX_FOLDER_PATHS {
                return None;
            }
            bytes.extend_from_slice(&(paths.len() as u32).to_le_bytes());
            for path in paths {
                let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
                if wide.is_empty() || wide.len() > HELPER_MAX_PATH_UNITS {
                    return None;
                }
                bytes.extend_from_slice(&(wide.len() as u32).to_le_bytes());
                for unit in wide {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
            }
        }
        HelperRequest::Search(query) => {
            if query.len() > HELPER_MAX_QUERY_BYTES {
                return None;
            }
            bytes.extend_from_slice(&(query.len() as u32).to_le_bytes());
            bytes.extend_from_slice(query.as_bytes());
        }
    }
    (bytes.len() <= HELPER_MAX_FRAME).then_some(bytes)
}

#[cfg(windows)]
struct HelperJob(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for HelperJob {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn reap_helper_resources(
    mut child: std::process::Child,
    slot: async_lock::SemaphoreGuardArc,
    threads: Vec<thread::JoinHandle<()>>,
    wait: impl FnOnce(&mut std::process::Child) -> std::io::Result<std::process::ExitStatus>,
) {
    if wait(&mut child).is_err() {
        // wait に失敗した場合は終了を確認できていないため、slot と child を保持する。
        HELPER_DISABLED.store(true, Ordering::Release);
        std::mem::forget((child, slot, threads));
        return;
    }
    for thread in threads {
        let _ = thread.join();
    }
    drop(slot);
}

#[cfg(windows)]
fn reap_failed_launch(
    mut child: std::process::Child,
    job: HelperJob,
    slot: async_lock::SemaphoreGuardArc,
    threads: Vec<thread::JoinHandle<()>>,
) {
    let _ = child.kill();
    drop(job);
    drop(child.stdin.take());
    drop(child.stdout.take());
    let deadline = Instant::now() + TERMINATE_CONFIRM_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                for thread in threads {
                    let _ = thread.join();
                }
                drop(slot);
                return;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => break,
        }
    }
    HELPER_DISABLED.store(true, Ordering::Release);
    let retained = Arc::new(std::sync::Mutex::new(Some((child, slot, threads))));
    let reaper_state = retained.clone();
    if thread::Builder::new()
        .name("everything-helper-launch-reaper".into())
        .spawn(move || {
            if let Some((child, slot, threads)) =
                reaper_state.lock().ok().and_then(|mut state| state.take())
            {
                reap_helper_resources(child, slot, threads, |child| child.wait());
            }
        })
        .is_err()
    {
        std::mem::forget(retained);
    }
}

#[cfg(windows)]
fn supervise_helper(
    slot: async_lock::SemaphoreGuardArc,
    request: HelperRequest,
    revision: ProviderRevision,
    deadline: Instant,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<HelperReply, WorkerError> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(executable);
    command.arg(HELPER_SWITCH);
    supervise_helper_with_command(slot, request, revision, deadline, cancelled, command, true)
}

#[cfg(windows)]
fn supervise_helper_with_command(
    slot: async_lock::SemaphoreGuardArc,
    request: HelperRequest,
    revision: ProviderRevision,
    deadline: Instant,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    command: Command,
    expect_response: bool,
) -> Result<HelperReply, WorkerError> {
    supervise_helper_with_command_and_diagnostic(
        slot,
        request,
        revision,
        deadline,
        cancelled,
        command,
        expect_response,
        None,
    )
}

#[cfg(windows)]
fn supervise_helper_with_command_and_diagnostic(
    slot: async_lock::SemaphoreGuardArc,
    request: HelperRequest,
    revision: ProviderRevision,
    deadline: Instant,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    command: Command,
    expect_response: bool,
    diagnostic: Option<Arc<Mutex<Option<Vec<u8>>>>>,
) -> Result<HelperReply, WorkerError> {
    supervise_helper_with_deadline_and_diagnostic(
        slot,
        request,
        revision,
        Some(deadline),
        cancelled,
        command,
        expect_response,
        diagnostic,
    )
}

#[cfg(windows)]
#[cfg(test)]
fn supervise_helper_with_deadline(
    slot: async_lock::SemaphoreGuardArc,
    request: HelperRequest,
    revision: ProviderRevision,
    deadline: Option<Instant>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    command: Command,
    expect_response: bool,
) -> Result<HelperReply, WorkerError> {
    supervise_helper_with_deadline_and_diagnostic(
        slot,
        request,
        revision,
        deadline,
        cancelled,
        command,
        expect_response,
        None,
    )
}

#[cfg(windows)]
fn supervise_helper_with_deadline_and_diagnostic(
    slot: async_lock::SemaphoreGuardArc,
    request: HelperRequest,
    revision: ProviderRevision,
    deadline: Option<Instant>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    mut command: Command,
    expect_response: bool,
    diagnostic: Option<Arc<Mutex<Option<Vec<u8>>>>>,
) -> Result<HelperReply, WorkerError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    if HELPER_DISABLED.load(Ordering::Acquire) {
        drop(slot);
        return Err(WorkerError::Unavailable);
    }
    let request_id = next_helper_id();
    let input =
        encode_helper_request(&request, request_id, revision).ok_or("invalid helper request")?;
    let job = HelperJob(unsafe { CreateJobObjectW(None, None) }.map_err(|e| e.to_string())?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if let Err(error) = configured {
        drop(slot);
        return Err(error.to_string().into());
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(if expect_response {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let assigned = unsafe {
        AssignProcessToJobObject(
            job.0,
            windows::Win32::Foundation::HANDLE(child.as_raw_handle() as _),
        )
    };
    if let Err(error) = assigned {
        reap_failed_launch(child, job, slot, Vec::new());
        return Err(error.to_string().into());
    }
    let Some(mut stdin) = child.stdin.take() else {
        reap_failed_launch(child, job, slot, Vec::new());
        return Err(WorkerError::Unavailable);
    };
    let stdout = if expect_response {
        let Some(stdout) = child.stdout.take() else {
            drop(stdin);
            reap_failed_launch(child, job, slot, Vec::new());
            return Err(WorkerError::Unavailable);
        };
        Some(stdout)
    } else {
        None
    };
    let (write_tx, write_rx) = mpsc::sync_channel(1);
    let writer = match thread::Builder::new()
        .name("everything-helper-request".into())
        .spawn(move || {
            let result = stdin
                .write_all(&(input.len() as u32).to_le_bytes())
                .and_then(|_| stdin.write_all(&input))
                .and_then(|_| stdin.flush());
            let _ = write_tx.send(result.map_err(|e| e.to_string()));
        }) {
        Ok(writer) => writer,
        Err(error) => {
            reap_failed_launch(child, job, slot, Vec::new());
            return Err(error.to_string().into());
        }
    };
    let (read_rx, reader) = if let Some(stdout) = stdout {
        let (read_tx, read_rx) = mpsc::sync_channel(1);
        let read_diagnostic = diagnostic.clone();
        let reader = match thread::Builder::new()
            .name("everything-helper-response".into())
            .spawn(move || {
                let mut stdout = stdout;
                let mut length = [0u8; 4];
                let result = stdout.read_exact(&mut length).and_then(|_| {
                    if let Some(diagnostic) = read_diagnostic.as_ref()
                        && let Ok(mut captured) = diagnostic.lock()
                    {
                        *captured = Some(length.to_vec());
                    }
                    let length = u32::from_le_bytes(length) as usize;
                    if length > HELPER_MAX_FRAME {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "oversized helper reply",
                        ));
                    }
                    let mut frame = vec![0; length];
                    stdout.read_exact(&mut frame)?;
                    if let Some(diagnostic) = read_diagnostic.as_ref()
                        && let Ok(mut captured) = diagnostic.lock()
                    {
                        *captured = Some(frame.clone());
                    }
                    Ok(frame)
                });
                if let Err(error) = &result
                    && let Some(diagnostic) = read_diagnostic.as_ref()
                    && let Ok(mut captured) = diagnostic.lock()
                {
                    *captured = Some(
                        format!("stdout reader error: {error}; prefix={length:02x?}").into_bytes(),
                    );
                }
                let _ = read_tx.send(result.map_err(|e| e.to_string()));
            }) {
            Ok(reader) => reader,
            Err(error) => {
                reap_failed_launch(child, job, slot, vec![writer]);
                return Err(error.to_string().into());
            }
        };
        (Some(read_rx), Some(reader))
    } else {
        (None, None)
    };
    let mut result = loop {
        if cancelled.load(Ordering::Acquire)
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            break Err(if cancelled.load(Ordering::Acquire) {
                WorkerError::Cancelled
            } else {
                WorkerError::Timeout
            });
        }
        if let Ok(Err(_error)) = write_rx.try_recv() {
            break Err(WorkerError::Unavailable);
        }
        if let Some(read_rx) = &read_rx {
            match read_rx.try_recv() {
                Ok(Ok(frame)) => {
                    break decode_helper_reply(&frame, &request, request_id, revision);
                }
                Ok(Err(_)) => break Err(WorkerError::Rejected),
                Err(mpsc::TryRecvError::Disconnected | mpsc::TryRecvError::Empty) => {}
            }
        }
        match child.try_wait() {
            Ok(Some(_)) => break Err(WorkerError::Unavailable),
            Err(error) => break Err(error.to_string().into()),
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    };
    if result.is_ok() {
        loop {
            if cancelled.load(Ordering::Acquire)
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                result = Err(if cancelled.load(Ordering::Acquire) {
                    WorkerError::Cancelled
                } else {
                    WorkerError::Timeout
                });
                break;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = writer.join();
                    if let Some(reader) = reader {
                        let _ = reader.join();
                    }
                    drop(job);
                    drop(slot);
                    return if status.success() {
                        result
                    } else {
                        Err(WorkerError::Unavailable)
                    };
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    result = Err(error.to_string().into());
                    break;
                }
            }
        }
    }
    let _ = child.kill();
    drop(job);
    let terminate_deadline = Instant::now() + TERMINATE_CONFIRM_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let _ = writer.join();
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
                drop(slot);
                return result;
            }
            Ok(None) if Instant::now() < terminate_deadline => {
                thread::sleep(Duration::from_millis(5))
            }
            _ => {
                HELPER_DISABLED.store(true, Ordering::Release);
                // Keep the permit until this process is actually reaped. The Job Object
                // already received kill-on-close; this reaper is the last bounded owner.
                let retained = Arc::new(std::sync::Mutex::new(Some((child, slot, writer, reader))));
                let reaper_state = retained.clone();
                let reaper = thread::Builder::new()
                    .name("everything-helper-reaper".into())
                    .spawn(move || {
                        let resources = reaper_state.lock().ok().and_then(|mut state| state.take());
                        let Some((child, slot, writer, reader)) = resources else {
                            return;
                        };
                        let mut threads = vec![writer];
                        if let Some(reader) = reader {
                            threads.push(reader);
                        }
                        reap_helper_resources(child, slot, threads, |child| child.wait());
                    });
                if reaper.is_err() {
                    // Thread creation failure must not return the capacity while the child may live.
                    // Retain the process handle and permit for process lifetime; launches are disabled.
                    std::mem::forget(retained);
                }
                return Err(WorkerError::Unavailable);
            }
        }
    }
}

#[cfg(windows)]
fn decode_helper_reply(
    frame: &[u8],
    request: &HelperRequest,
    id: u64,
    revision: ProviderRevision,
) -> Result<HelperReply, WorkerError> {
    use std::os::windows::ffi::OsStringExt;
    struct Cursor<'a> {
        bytes: &'a [u8],
        at: usize,
    }
    impl<'a> Cursor<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
            let end = self.at.checked_add(n).ok_or("reply length overflow")?;
            let part = self
                .bytes
                .get(self.at..end)
                .ok_or("truncated helper reply")?;
            self.at = end;
            Ok(part)
        }
        fn u8(&mut self) -> Result<u8, String> {
            Ok(self.take(1)?[0])
        }
        fn u16(&mut self) -> Result<u16, String> {
            Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
        }
        fn u32(&mut self) -> Result<u32, String> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }
        fn u64(&mut self) -> Result<u64, String> {
            Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }
        fn path(&mut self) -> Result<PathBuf, String> {
            let n = self.u32()? as usize;
            if n == 0 || n > HELPER_MAX_PATH_UNITS {
                return Err("invalid helper path length".into());
            }
            let raw = self.take(n.checked_mul(2).ok_or("path length overflow")?)?;
            let wide = raw
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .collect::<Vec<_>>();
            Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
        }
    }
    let decoded = (|| -> Result<HelperReply, HelperDecodeError> {
        let mut c = Cursor {
            bytes: frame,
            at: 0,
        };
        if c.take(4)? != b"EIPC" || c.u16()? != HELPER_PROTOCOL_VERSION {
            return Err("helper protocol mismatch".into());
        }
        let kind = c.u8()?;
        if c.u64()? != id || c.u64()? != revision.0 {
            return Err("helper identity mismatch".into());
        }
        if c.u8()? != 0 {
            return Err(HelperDecodeError::Provider);
        }
        let reply = match (kind, request) {
            (1, HelperRequest::FolderSizes(paths)) => {
                let count = c.u32()? as usize;
                if count != paths.len() || count > HELPER_MAX_FOLDER_PATHS {
                    return Err("folder reply count mismatch".into());
                }
                let mut sizes = Vec::with_capacity(count);
                for _ in 0..count {
                    match c.u8()? {
                        0 => sizes.push(None),
                        1 => sizes.push(Some(c.u64()?)),
                        _ => return Err("invalid folder result".into()),
                    }
                }
                HelperReply::FolderSizes(sizes)
            }
            (2, HelperRequest::Search(_)) => {
                let count = c.u32()? as usize;
                if count > SEARCH_MAX_RESULTS as usize {
                    return Err("too many search results".into());
                }
                let mut results = Vec::with_capacity(count);
                for _ in 0..count {
                    let path = c.path()?;
                    let name_path = c.path()?;
                    let name = name_path
                        .into_os_string()
                        .into_string()
                        .map_err(|_| "invalid search result name")?;
                    let is_folder = match c.u8()? {
                        0 => false,
                        1 => true,
                        _ => return Err("invalid search kind".into()),
                    };
                    let size = match c.u8()? {
                        0 => None,
                        1 => Some(c.u64()?),
                        _ => return Err("invalid search size".into()),
                    };
                    results.push(SearchResult {
                        path,
                        name,
                        is_folder,
                        size,
                        revision,
                    });
                }
                HelperReply::Search(results)
            }
            _ => return Err("helper reply kind mismatch".into()),
        };
        if c.at != frame.len() {
            return Err("trailing helper reply bytes".into());
        }
        Ok(reply)
    })();
    decoded.map_err(|error| match error {
        HelperDecodeError::Provider => WorkerError::Unavailable,
        HelperDecodeError::Protocol => WorkerError::Rejected,
    })
}

#[cfg(windows)]
fn write_wide_path(output: &mut Vec<u8>, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.len() > HELPER_MAX_PATH_UNITS {
        return Err("invalid helper output path".into());
    }
    output.extend_from_slice(&(wide.len() as u32).to_le_bytes());
    for unit in wide {
        output.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

#[cfg(windows)]
fn helper_compute(request: HelperRequest, revision: ProviderRevision) -> Result<Vec<u8>, String> {
    let kind = match &request {
        HelperRequest::FolderSizes(_) => 1,
        HelperRequest::Search(_) => 2,
    };
    let result = match request {
        HelperRequest::FolderSizes(paths) => {
            let mut out = Vec::new();
            out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
            for path in paths {
                match folder_ipc(path) {
                    Ok(size) => {
                        out.push(1);
                        out.extend_from_slice(&size.to_le_bytes());
                    }
                    Err(_) => out.push(0),
                }
            }
            out
        }
        HelperRequest::Search(query) => {
            let results = search_ipc(&query, revision)?;
            let mut out = Vec::new();
            out.extend_from_slice(&(results.len() as u32).to_le_bytes());
            for result in results {
                write_wide_path(&mut out, &result.path)?;
                write_wide_path(&mut out, Path::new(&result.name))?;
                out.push(u8::from(result.is_folder));
                match result.size {
                    Some(size) => {
                        out.push(1);
                        out.extend_from_slice(&size.to_le_bytes());
                    }
                    None => out.push(0),
                }
            }
            out
        }
    };
    if result.len() > HELPER_MAX_FRAME.saturating_sub(23) {
        return Err("helper response too large".into());
    }
    let mut frame = b"EIPC".to_vec();
    frame.extend_from_slice(&HELPER_PROTOCOL_VERSION.to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(&next_helper_id().to_le_bytes()); // overwritten with request id by helper entrypoint
    frame.extend_from_slice(&revision.0.to_le_bytes());
    frame.push(0);
    frame.extend_from_slice(&result);
    Ok(frame)
}

#[cfg(windows)]
pub fn run_helper_from_process_args() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(HELPER_SWITCH)) {
        return false;
    }
    let result = (|| -> Result<(), String> {
        #[cfg(feature = "everything-private-ipc-test")]
        if let Some(receipt_path) = std::env::var_os(PRIVATE_IPC_TEST_PREFLIGHT_ENV) {
            std::fs::write(receipt_path, PRIVATE_IPC_TEST_PREFLIGHT_RECEIPT)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let mut stdin = std::io::stdin().lock();
        let mut len = [0; 4];
        stdin.read_exact(&mut len).map_err(|e| e.to_string())?;
        let len = u32::from_le_bytes(len) as usize;
        if len > HELPER_MAX_FRAME || len < 23 {
            return Err("invalid helper request frame".into());
        }
        let mut bytes = vec![0; len];
        stdin.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        let (request, id, revision) = decode_helper_request(&bytes)?;
        #[cfg(feature = "everything-private-ipc-test")]
        std::fs::write(
            std::env::var_os(PRIVATE_IPC_TEST_HELPER_PID_ENV)
                .ok_or("private IPC test helper PID path is required")?,
            std::process::id().to_string(),
        )
        .map_err(|error| error.to_string())?;
        let kind = match &request {
            HelperRequest::FolderSizes(_) => 1,
            HelperRequest::Search(_) => 2,
        };
        let mut frame = helper_compute(request, revision).unwrap_or_else(|_| {
            let mut failed = b"EIPC".to_vec();
            failed.extend_from_slice(&HELPER_PROTOCOL_VERSION.to_le_bytes());
            failed.push(kind);
            failed.extend_from_slice(&id.to_le_bytes());
            failed.extend_from_slice(&revision.0.to_le_bytes());
            failed.push(1);
            failed
        });
        frame[7..15].copy_from_slice(&id.to_le_bytes());
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&(frame.len() as u32).to_le_bytes())
            .and_then(|_| stdout.write_all(&frame))
            .and_then(|_| stdout.flush())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        std::process::exit(1);
    }
    std::process::exit(0)
}

#[cfg(not(windows))]
pub fn run_helper_from_process_args() -> bool {
    false
}

#[cfg(windows)]
fn decode_helper_request(bytes: &[u8]) -> Result<(HelperRequest, u64, ProviderRevision), String> {
    use std::os::windows::ffi::OsStringExt;
    let mut at = 0usize;
    fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], String> {
        let end = at.checked_add(n).ok_or("request overflow")?;
        let v = bytes.get(*at..end).ok_or("truncated request")?;
        *at = end;
        Ok(v)
    }
    fn u32v(bytes: &[u8], at: &mut usize) -> Result<u32, String> {
        Ok(u32::from_le_bytes(take(bytes, at, 4)?.try_into().unwrap()))
    }
    if take(bytes, &mut at, 4)? != b"EIPC"
        || u16::from_le_bytes(take(bytes, &mut at, 2)?.try_into().unwrap())
            != HELPER_PROTOCOL_VERSION
    {
        return Err("request protocol mismatch".into());
    }
    let kind = take(bytes, &mut at, 1)?[0];
    let id = u64::from_le_bytes(take(bytes, &mut at, 8)?.try_into().unwrap());
    let revision = ProviderRevision(u64::from_le_bytes(
        take(bytes, &mut at, 8)?.try_into().unwrap(),
    ));
    let request = match kind {
        1 => {
            let count = u32v(bytes, &mut at)? as usize;
            if count == 0 || count > HELPER_MAX_FOLDER_PATHS {
                return Err("invalid folder request count".into());
            }
            let mut paths = Vec::with_capacity(count);
            for _ in 0..count {
                let n = u32v(bytes, &mut at)? as usize;
                if n == 0 || n > HELPER_MAX_PATH_UNITS {
                    return Err("invalid folder path length".into());
                }
                let raw = take(bytes, &mut at, n.checked_mul(2).ok_or("path overflow")?)?;
                let wide = raw
                    .chunks_exact(2)
                    .map(|p| u16::from_le_bytes([p[0], p[1]]))
                    .collect::<Vec<_>>();
                paths.push(PathBuf::from(std::ffi::OsString::from_wide(&wide)));
            }
            HelperRequest::FolderSizes(paths)
        }
        2 => {
            let n = u32v(bytes, &mut at)? as usize;
            if n > HELPER_MAX_QUERY_BYTES {
                return Err("query too large".into());
            }
            HelperRequest::Search(
                String::from_utf8(take(bytes, &mut at, n)?.to_vec())
                    .map_err(|_| "invalid search query utf8")?,
            )
        }
        _ => return Err("unknown helper request kind".into()),
    };
    if at != bytes.len() {
        return Err("trailing helper request data".into());
    }
    Ok((request, id, revision))
}

#[cfg(not(windows))]
fn supervise_helper(
    slot: async_lock::SemaphoreGuardArc,
    _request: HelperRequest,
    _revision: ProviderRevision,
    _deadline: Instant,
    _cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<HelperReply, WorkerError> {
    drop(slot);
    Err(WorkerError::Unavailable)
}

#[cfg(target_os = "windows")]
fn folder_ipc(path: PathBuf) -> Result<u64, String> {
    everything_ipc::folder::size::get_folder_size(&path)
        .call()
        .map_err(|error| error.to_string())
}
#[cfg(not(target_os = "windows"))]
fn folder_ipc(_path: PathBuf) -> Result<u64, String> {
    Err("Everything is only available on Windows".into())
}

pub async fn query_folder_sizes(
    executor: gpui::BackgroundExecutor,
    probes: Vec<FolderSizeProbe>,
    cancelled: &std::sync::atomic::AtomicBool,
    revision: ProviderRevision,
    bypass_cache: bool,
) -> FolderSizeOutcome {
    query_folder_sizes_with_batch_runner(executor, probes, cancelled, revision, bypass_cache, None)
        .await
}

type FolderSizeBatchRunner = Arc<
    dyn Fn(
            async_lock::SemaphoreGuardArc,
            HelperRequest,
            ProviderRevision,
            Instant,
            Arc<std::sync::atomic::AtomicBool>,
        ) -> Result<HelperReply, WorkerError>
        + Send
        + Sync,
>;

#[cfg(test)]
async fn query_folder_sizes_with_test_batch_runner(
    executor: gpui::BackgroundExecutor,
    probes: Vec<FolderSizeProbe>,
    cancelled: &std::sync::atomic::AtomicBool,
    revision: ProviderRevision,
    bypass_cache: bool,
    runner: FolderSizeBatchRunner,
) -> FolderSizeOutcome {
    query_folder_sizes_with_batch_runner(
        executor,
        probes,
        cancelled,
        revision,
        bypass_cache,
        Some(runner),
    )
    .await
}

async fn query_folder_sizes_with_batch_runner(
    executor: gpui::BackgroundExecutor,
    probes: Vec<FolderSizeProbe>,
    cancelled: &std::sync::atomic::AtomicBool,
    revision: ProviderRevision,
    bypass_cache: bool,
    batch_runner: Option<FolderSizeBatchRunner>,
) -> FolderSizeOutcome {
    use crate::directory::FolderSizeProvenance;
    let cache_generation = probes
        .first()
        .map(|probe| probe.cache_generation)
        .unwrap_or_else(current_folder_size_cache_generation);
    let deadline = Instant::now() + FOLDER_SIZE_DEADLINE;
    let mut results = Vec::new();
    let mut retry_invalidated = false;
    let mut pending = Vec::new();
    for probe in probes {
        let request_order = provider()
            .next_folder_size_request_order
            .fetch_add(1, Ordering::Relaxed);
        if !probe.freshness.is_current() {
            retry_invalidated = true;
            continue;
        }
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return folder_size_outcome(
                revision,
                cache_generation,
                results,
                retry_invalidated,
                if cancelled.load(Ordering::Acquire) {
                    ProviderTerminal::Cancelled
                } else {
                    ProviderTerminal::Timeout
                },
            );
        }
        if !probe.source_identity.matches_path(&probe.path) {
            continue;
        }
        if probe.cache_generation != cache_generation {
            continue;
        }
        if let Some(fact) =
            provider().cached(&probe.path, &probe.source_identity, revision, !bypass_cache)
        {
            results.push(FolderSizeResult {
                key: probe.key,
                path: probe.path,
                fact,
                revision,
                cache_generation,
                freshness: probe.freshness,
            });
            continue;
        }
        pending.push((probe, request_order));
    }
    if pending.is_empty() {
        return folder_size_outcome(
            revision,
            cache_generation,
            results,
            retry_invalidated,
            ProviderTerminal::Completed,
        );
    }
    // 1通のprotocol上限は512件。受理済みlistingが大きい場合は、同じ期限内で逐次分割する。
    let mut pending = pending.into_iter();
    loop {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return folder_size_outcome(
                revision,
                cache_generation,
                results,
                retry_invalidated,
                if cancelled.load(Ordering::Acquire) {
                    ProviderTerminal::Cancelled
                } else {
                    ProviderTerminal::Timeout
                },
            );
        }
        if ProviderRevision::current() != revision {
            // Search失敗がbatch間に共有revisionを進めた場合、古い部分結果はcommitせず、
            // 現在のlistingだけ既存のcoalesced retryへ戻す。
            return folder_size_outcome(
                revision,
                cache_generation,
                results,
                true,
                ProviderTerminal::Completed,
            );
        }
        let batch = take_folder_size_batch(&mut pending);
        if batch.is_empty() {
            break;
        }
        let slot = match acquire_slot(&executor, cancelled, deadline).await {
            Ok(slot) => slot,
            Err(terminal) => {
                return folder_size_outcome(
                    revision,
                    cache_generation,
                    results,
                    retry_invalidated,
                    terminal,
                );
            }
        };
        let supervisor_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_cancel = supervisor_cancelled.clone();
        let batch_runner = batch_runner.clone();
        let paths = batch.iter().map(|(probe, _)| probe.path.clone()).collect();
        let request = HelperRequest::FolderSizes(paths);
        let (sender, receiver) = mpsc::channel();
        if thread::Builder::new()
            .name("everything-folder-batch".into())
            .spawn(move || {
                let reply = if let Some(batch_runner) = batch_runner {
                    batch_runner(slot, request, revision, deadline, child_cancel)
                } else {
                    supervise_helper(slot, request, revision, deadline, child_cancel)
                        .map_err(|error| error)
                };
                let _ = sender.send(reply);
            })
            .is_err()
        {
            return folder_size_outcome(
                provider().invalidate(revision),
                cache_generation,
                results,
                retry_invalidated,
                ProviderTerminal::Unavailable,
            );
        }
        match wait_for_worker(
            &executor,
            receiver,
            cancelled,
            deadline,
            supervisor_cancelled.as_ref(),
        )
        .await
        {
            Ok(HelperReply::FolderSizes(sizes)) => {
                for ((probe, request_order), bytes) in batch.into_iter().zip(sizes) {
                    if bytes.is_none() {
                        return folder_size_outcome(
                            provider().invalidate(revision),
                            cache_generation,
                            results,
                            retry_invalidated,
                            ProviderTerminal::Unavailable,
                        );
                    }
                    if !probe.freshness.is_current() {
                        retry_invalidated = true;
                        continue;
                    }
                    if cancelled.load(Ordering::Acquire)
                        || ProviderRevision::current() != revision
                        || !probe.source_identity.matches_path(&probe.path)
                    {
                        continue;
                    }
                    let fact = FolderSizeFact {
                        bytes: bytes.unwrap(),
                        provenance: FolderSizeProvenance::EverythingIndex,
                    };
                    provider().insert(
                        &probe.path,
                        fact,
                        probe.source_identity,
                        revision,
                        &probe.freshness,
                        request_order,
                        cancelled,
                    );
                    results.push(FolderSizeResult {
                        key: probe.key,
                        path: probe.path,
                        fact,
                        revision,
                        cache_generation,
                        freshness: probe.freshness,
                    });
                }
            }
            Ok(HelperReply::Search(_)) => {
                return folder_size_outcome(
                    revision,
                    cache_generation,
                    results,
                    retry_invalidated,
                    ProviderTerminal::Rejected,
                );
            }
            Err(ProviderTerminal::Cancelled) => {
                return folder_size_outcome(
                    revision,
                    cache_generation,
                    results,
                    retry_invalidated,
                    ProviderTerminal::Cancelled,
                );
            }
            Err(ProviderTerminal::Timeout) => {
                return folder_size_outcome(
                    revision,
                    cache_generation,
                    results,
                    retry_invalidated,
                    ProviderTerminal::Timeout,
                );
            }
            Err(ProviderTerminal::Rejected) => {
                return folder_size_outcome(
                    revision,
                    cache_generation,
                    results,
                    retry_invalidated,
                    ProviderTerminal::Rejected,
                );
            }
            Err(ProviderTerminal::Unavailable) => {
                return folder_size_outcome(
                    provider().invalidate(revision),
                    cache_generation,
                    results,
                    retry_invalidated,
                    ProviderTerminal::Unavailable,
                );
            }
            Err(ProviderTerminal::Completed) => unreachable!(),
        }
    }
    folder_size_outcome(
        revision,
        cache_generation,
        results,
        retry_invalidated,
        ProviderTerminal::Completed,
    )
}

fn take_folder_size_batch<T>(pending: &mut impl Iterator<Item = T>) -> Vec<T> {
    pending.by_ref().take(HELPER_MAX_FOLDER_PATHS).collect()
}

fn folder_size_outcome(
    revision: ProviderRevision,
    cache_generation: FolderSizeCacheGeneration,
    results: Vec<FolderSizeResult>,
    retry_invalidated: bool,
    terminal: ProviderTerminal,
) -> FolderSizeOutcome {
    FolderSizeOutcome {
        revision,
        cache_generation,
        results,
        retry_invalidated,
        terminal,
    }
}

pub async fn search(
    executor: gpui::BackgroundExecutor,
    query: String,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    revision: ProviderRevision,
) -> SearchOutcome {
    search_with_command_factory(executor, query, cancelled, revision, None).await
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestSearchCommand {
    pub(crate) factory: Arc<dyn Fn() -> Command + Send + Sync>,
    pub(crate) expect_response: bool,
    pub(crate) captured_reply: Arc<Mutex<Option<Vec<u8>>>>,
    pub(crate) terminal_observer:
        Option<Arc<dyn Fn(ProviderTerminal, ProviderRevision) + Send + Sync>>,
}

type SearchCommandFactory = Arc<dyn Fn() -> Command + Send + Sync>;

#[cfg(test)]
pub(crate) async fn search_with_test_command(
    executor: gpui::BackgroundExecutor,
    query: String,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    revision: ProviderRevision,
    command: TestSearchCommand,
) -> SearchOutcome {
    let outcome = search_with_command_factory(
        executor,
        query,
        cancelled,
        revision,
        Some((
            command.factory,
            command.expect_response,
            command.captured_reply,
        )),
    )
    .await;
    if let Some(observer) = command.terminal_observer {
        observer(outcome.terminal, outcome.revision);
    }
    outcome
}

async fn search_with_command_factory(
    executor: gpui::BackgroundExecutor,
    query: String,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    revision: ProviderRevision,
    command_factory: Option<(SearchCommandFactory, bool, Arc<Mutex<Option<Vec<u8>>>>)>,
) -> SearchOutcome {
    let deadline = Instant::now() + FOLDER_SIZE_DEADLINE;
    let slot = match acquire_slot(&executor, cancelled.as_ref(), deadline).await {
        Ok(slot) => slot,
        Err(ProviderTerminal::Cancelled) => {
            return SearchOutcome {
                revision,
                results: Vec::new(),
                terminal: ProviderTerminal::Cancelled,
                error: None,
            };
        }
        Err(ProviderTerminal::Timeout) => {
            return SearchOutcome {
                revision: provider().invalidate(revision),
                results: Vec::new(),
                terminal: ProviderTerminal::Timeout,
                error: Some("Everything query timed out".into()),
            };
        }
        Err(
            ProviderTerminal::Completed
            | ProviderTerminal::Unavailable
            | ProviderTerminal::Rejected,
        ) => unreachable!(),
    };
    let supervisor_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child_cancel = supervisor_cancelled.clone();
    let (sender, receiver) = mpsc::channel();
    if thread::Builder::new()
        .name("everything-search".into())
        .spawn(move || {
            #[cfg(windows)]
            let result =
                if let Some((command_factory, expect_response, captured_reply)) = command_factory {
                    supervise_helper_with_command_and_diagnostic(
                        slot,
                        HelperRequest::Search(query),
                        revision,
                        deadline,
                        child_cancel,
                        command_factory(),
                        expect_response,
                        Some(captured_reply),
                    )
                } else {
                    supervise_helper(
                        slot,
                        HelperRequest::Search(query),
                        revision,
                        deadline,
                        child_cancel,
                    )
                };
            #[cfg(not(windows))]
            let result = {
                let _ = command_factory;
                supervise_helper(
                    slot,
                    HelperRequest::Search(query),
                    revision,
                    deadline,
                    child_cancel,
                )
            };
            let _ = sender.send(result);
        })
        .is_err()
    {
        return SearchOutcome {
            revision: provider().invalidate(revision),
            results: Vec::new(),
            terminal: ProviderTerminal::Unavailable,
            error: Some("Everything IPC unavailable".into()),
        };
    }
    match wait_for_worker(
        &executor,
        receiver,
        cancelled.as_ref(),
        deadline,
        supervisor_cancelled.as_ref(),
    )
    .await
    {
        Ok(HelperReply::Search(results)) => SearchOutcome {
            revision,
            results,
            terminal: ProviderTerminal::Completed,
            error: None,
        },
        Ok(HelperReply::FolderSizes(_)) => SearchOutcome {
            revision,
            results: Vec::new(),
            terminal: ProviderTerminal::Rejected,
            error: Some("Everything helper reply kind mismatch".into()),
        },
        Err(ProviderTerminal::Cancelled) => SearchOutcome {
            revision,
            results: Vec::new(),
            terminal: ProviderTerminal::Cancelled,
            error: None,
        },
        Err(terminal @ ProviderTerminal::Timeout)
        | Err(terminal @ ProviderTerminal::Unavailable) => SearchOutcome {
            revision: provider().invalidate(revision),
            results: Vec::new(),
            terminal,
            error: Some(
                if terminal == ProviderTerminal::Timeout {
                    "Everything query timed out"
                } else {
                    "Everything IPC unavailable"
                }
                .into(),
            ),
        },
        Err(ProviderTerminal::Rejected) => SearchOutcome {
            revision,
            results: Vec::new(),
            terminal: ProviderTerminal::Rejected,
            error: Some("Everything helper protocol rejected".into()),
        },
        Err(ProviderTerminal::Completed) => unreachable!(),
    }
}

#[cfg(target_os = "windows")]
fn search_ipc(query: &str, revision: ProviderRevision) -> Result<Vec<SearchResult>, String> {
    use everything_ipc::wm::{EverythingClient, RequestFlags, Sort};
    #[cfg(feature = "everything-private-ipc-test")]
    let client = {
        let instance = std::env::var(PRIVATE_IPC_TEST_INSTANCE_ENV)
            .map_err(|_| "private IPC test instance is required".to_owned())?;
        EverythingClient::with_instance(Some(&instance)).map_err(|error| error.to_string())?
    };
    #[cfg(not(feature = "everything-private-ipc-test"))]
    let client = EverythingClient::new().map_err(|error| error.to_string())?;
    let list = client
        .query_wait(query)
        .request_flags(RequestFlags::FileName | RequestFlags::Path | RequestFlags::Size)
        .sort(Sort::NameAscending)
        .max_results(SEARCH_MAX_RESULTS)
        .call()
        .map_err(|error| error.to_string())?;
    let mut results = Vec::new();
    for item in list.iter().take(SEARCH_MAX_RESULTS as usize) {
        let name = item
            .get_string(RequestFlags::FileName)
            .unwrap_or_default()
            .to_string();
        let parent = item
            .get_string(RequestFlags::Path)
            .unwrap_or_default()
            .to_string();
        let Some(path) = sanitize_search_path(&parent, &name) else {
            continue;
        };
        let is_folder = std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        results.push(SearchResult {
            path,
            name,
            is_folder,
            size: item.get_size(RequestFlags::Size),
            revision,
        });
    }
    Ok(results)
}

fn sanitize_search_path(parent: &str, name: &str) -> Option<PathBuf> {
    use std::path::Component;

    if parent.is_empty() || name.is_empty() {
        return None;
    }
    let parent = Path::new(parent);
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return None;
    }
    if !parent.is_absolute() {
        return None;
    }
    let path = parent.join(name);
    (path.is_absolute() && path.file_name().and_then(|value| value.to_str()) == Some(name))
        .then_some(path)
}
#[cfg(not(target_os = "windows"))]
fn search_ipc(_query: &str, _revision: ProviderRevision) -> Result<Vec<SearchResult>, String> {
    Err("Everything search is only available on Windows".into())
}

#[allow(dead_code)]
pub fn invalidate(expected: ProviderRevision) -> ProviderRevision {
    provider().invalidate(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::{EntryKind, FolderSizeFact, FolderSizeProvenance};

    #[cfg(windows)]
    const HELPER_TEST_READY_ENV: &str = "GPUI_FILER_EVERYTHING_HELPER_TEST_READY";
    #[cfg(windows)]
    const HELPER_TEST_RELEASE_ENV: &str = "GPUI_FILER_EVERYTHING_HELPER_TEST_RELEASE";

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an installed/running Everything service and a fresh release image"]
    fn everything_ipc_live_service_supervised_release_helper() {
        let executable = std::env::var_os("GPUI_FILER_EVERYTHING_TEST_EXECUTABLE")
            .expect("set GPUI_FILER_EVERYTHING_TEST_EXECUTABLE to the reviewed release exe");
        let fixture_root = std::env::var_os("GPUI_FILER_EVERYTHING_TEST_FIXTURE_ROOT")
            .expect("set GPUI_FILER_EVERYTHING_TEST_FIXTURE_ROOT to a prepared folder");
        let fixture_root = PathBuf::from(fixture_root);
        assert!(fixture_root.is_dir(), "fixture root must be a directory");

        let slots = Arc::new(async_lock::Semaphore::new(1));
        let revision = ProviderRevision::current();

        let search_started = Instant::now();
        let search_slot = slots.try_acquire_arc().expect("search slot available");
        let mut search_command = Command::new(&executable);
        search_command.arg(HELPER_SWITCH);
        let search_reply = supervise_helper_with_command(
            search_slot,
            HelperRequest::Search("gpui-filer".into()),
            revision,
            search_started + Duration::from_secs(5),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            search_command,
            true,
        )
        .expect("live Everything Search helper should complete successfully");
        let search_results = match search_reply {
            HelperReply::Search(results) => results,
            HelperReply::FolderSizes(_) => panic!("Search helper returned the wrong reply variant"),
        };
        assert!(search_results.len() <= SEARCH_MAX_RESULTS as usize);
        assert!(
            search_results
                .iter()
                .all(|result| result.revision == revision)
        );

        let folder_started = Instant::now();
        let folder_slot = slots.try_acquire_arc().expect("folder-size slot available");
        let mut folder_command = Command::new(&executable);
        folder_command.arg(HELPER_SWITCH);
        let folder_reply = supervise_helper_with_command(
            folder_slot,
            HelperRequest::FolderSizes(vec![fixture_root]),
            revision,
            folder_started + Duration::from_secs(5),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            folder_command,
            true,
        )
        .expect("live Everything folder-size helper should complete successfully");
        let folder_sizes = match folder_reply {
            HelperReply::FolderSizes(sizes) => sizes,
            HelperReply::Search(_) => panic!("folder-size helper returned the wrong reply variant"),
        };
        assert_eq!(folder_sizes.len(), 1);
        assert!(
            folder_sizes[0].is_some(),
            "fixture folder size must be indexed"
        );
        assert_eq!(ProviderRevision::current(), revision);
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_hold_open_fixture() {
        let Ok(ready_path) = std::env::var(HELPER_TEST_READY_ENV) else {
            return;
        };
        use std::io::Read;
        let mut stdin = std::io::stdin().lock();
        let mut length = [0; 4];
        stdin.read_exact(&mut length).unwrap();
        let mut request = vec![0; u32::from_le_bytes(length) as usize];
        stdin.read_exact(&mut request).unwrap();
        std::fs::write(ready_path, std::process::id().to_string()).unwrap();
        if let Ok(release_path) = std::env::var(HELPER_TEST_RELEASE_ENV) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !Path::new(&release_path).exists() {
                assert!(Instant::now() < deadline, "held helper release timed out");
                thread::sleep(Duration::from_millis(5));
            }
        } else {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
    }

    #[cfg(windows)]
    const HELPER_TEST_ECHO_READY_ENV: &str = "GPUI_FILER_EVERYTHING_HELPER_ECHO_READY";
    #[cfg(windows)]
    const HELPER_TEST_ECHO_QUERY_ENV: &str = "GPUI_FILER_EVERYTHING_HELPER_ECHO_QUERY";
    #[cfg(windows)]
    const HELPER_TEST_ECHO_PATH_ENV: &str = "GPUI_FILER_EVERYTHING_HELPER_ECHO_PATH";
    #[cfg(windows)]
    const HELPER_TEST_ECHO_RELEASE_ENV: &str = "GPUI_FILER_EVERYTHING_HELPER_ECHO_RELEASE";

    #[cfg(windows)]
    const BLOCKED_IPC_INSTANCE_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_INSTANCE";
    #[cfg(windows)]
    const BLOCKED_IPC_ENTERED_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_ENTERED";
    #[cfg(windows)]
    const BLOCKED_IPC_RELEASE_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_RELEASE";
    #[cfg(windows)]
    const BLOCKED_IPC_SERVER_READY_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_SERVER_READY";
    #[cfg(windows)]
    const BLOCKED_IPC_HELPER_READY_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_HELPER_READY";
    #[cfg(windows)]
    const BLOCKED_IPC_HELPER_GATE_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_HELPER_GATE";
    #[cfg(windows)]
    const BLOCKED_IPC_SERVER_DIAGNOSTIC_ENV: &str =
        "GPUI_FILER_EVERYTHING_BLOCKED_IPC_SERVER_DIAGNOSTIC";
    #[cfg(windows)]
    const BLOCKED_IPC_QUERY_ENV: &str = "GPUI_FILER_EVERYTHING_BLOCKED_IPC_QUERY";

    #[cfg(windows)]
    static BLOCKED_IPC_ENTERED_HANDLE: std::sync::atomic::AtomicIsize =
        std::sync::atomic::AtomicIsize::new(0);
    #[cfg(windows)]
    static BLOCKED_IPC_RELEASE_HANDLE: std::sync::atomic::AtomicIsize =
        std::sync::atomic::AtomicIsize::new(0);

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_private_blocked_server_fixture() {
        use windows::Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            System::{DataExchange::COPYDATASTRUCT, Threading::WaitForSingleObject},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
                RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COPYDATA,
                WNDCLASSW,
            },
        };

        unsafe extern "system" fn procedure(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message != WM_COPYDATA {
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let data = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
            let valid = if data.dwData == 18
                && !data.lpData.is_null()
                && (30..=65_536).contains(&data.cbData)
                && data.cbData % 2 == 0
            {
                let bytes = unsafe {
                    std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize)
                };
                let query =
                    std::env::var(BLOCKED_IPC_QUERY_ENV).expect("private fixture query missing");
                let encoded = query
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>();
                let end = 28usize.saturating_add(encoded.len());
                end + 2 <= bytes.len()
                    && bytes.get(28..end) == Some(encoded.as_slice())
                    && bytes.get(end..end + 2) == Some(&[0, 0])
            } else {
                false
            };
            if let Ok(path) = std::env::var(BLOCKED_IPC_SERVER_DIAGNOSTIC_ENV) {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(
                        file,
                        "wm_copydata dwData={} cbData={} valid={valid}",
                        data.dwData, data.cbData
                    );
                }
            }
            let entered = BLOCKED_IPC_ENTERED_HANDLE.load(Ordering::Acquire);
            let release = BLOCKED_IPC_RELEASE_HANDLE.load(Ordering::Acquire);
            if !valid || entered == 0 || release == 0 {
                return LRESULT(0);
            }

            // COPYDATA の借用 payload は同期 send の間だけ有効。検証してから
            // 外部から解放可能な event を通知し、WndProc 内で待つ。
            unsafe {
                let _ = windows::Win32::System::Threading::SetEvent(
                    windows::Win32::Foundation::HANDLE(entered as _),
                );
                let _ =
                    WaitForSingleObject(windows::Win32::Foundation::HANDLE(release as _), u32::MAX);
                PostQuitMessage(0);
            }
            LRESULT(1)
        }

        use windows::Win32::System::Threading::{
            EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS,
        };
        use windows::core::PCWSTR;

        let instance = std::env::var(BLOCKED_IPC_INSTANCE_ENV).unwrap();
        let class_name = format!("EVERYTHING_TASKBAR_NOTIFICATION({instance})");
        assert!(!instance.is_empty());
        assert_ne!(class_name, "EVERYTHING_TASKBAR_NOTIFICATION");
        assert_ne!(class_name, "EVERYTHING_TASKBAR_NOTIFICATION(1.5a)");
        let entered_name = std::env::var(BLOCKED_IPC_ENTERED_ENV).unwrap();
        let release_name = std::env::var(BLOCKED_IPC_RELEASE_ENV).unwrap();
        let entered_wide = entered_name.encode_utf16().chain([0]).collect::<Vec<_>>();
        let release_wide = release_name.encode_utf16().chain([0]).collect::<Vec<_>>();
        let sync = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
        let entered = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | sync,
                false,
                PCWSTR(entered_wide.as_ptr()),
            )
        }
        .expect("open parent entered event");
        let release = unsafe { OpenEventW(sync, false, PCWSTR(release_wide.as_ptr())) }
            .expect("open parent release event");
        BLOCKED_IPC_ENTERED_HANDLE.store(entered.0 as isize, Ordering::Release);
        BLOCKED_IPC_RELEASE_HANDLE.store(release.0 as isize, Ordering::Release);

        let class_wide = class_name.encode_utf16().chain([0]).collect::<Vec<_>>();
        let mut class = WNDCLASSW::default();
        class.lpfnWndProc = Some(procedure);
        class.lpszClassName = PCWSTR(class_wide.as_ptr());
        assert_ne!(
            unsafe { RegisterClassW(&class) },
            0,
            "register private IPC class"
        );
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(class_wide.as_ptr()),
                PCWSTR(class_wide.as_ptr()),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND::default(),
                None,
                None,
                None,
            )
        }
        .expect("create private Everything endpoint");
        assert!(!window.is_invalid());
        std::fs::write(
            std::env::var(BLOCKED_IPC_SERVER_READY_ENV).unwrap(),
            class_name,
        )
        .unwrap();
        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) }.0 > 0 {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(entered);
            let _ = windows::Win32::Foundation::CloseHandle(release);
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_private_blocked_search_helper_fixture() {
        use std::io::Read;
        use windows::{
            Win32::System::Threading::{
                OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, WaitForSingleObject,
            },
            core::PCWSTR,
        };

        let ready = std::env::var(BLOCKED_IPC_HELPER_READY_ENV).unwrap();
        std::fs::write(&ready, std::process::id().to_string()).unwrap();
        let gate_name = std::env::var(BLOCKED_IPC_HELPER_GATE_ENV).unwrap();
        let gate_wide = gate_name.encode_utf16().chain([0]).collect::<Vec<_>>();
        let gate = unsafe {
            OpenEventW(
                SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                false,
                PCWSTR(gate_wide.as_ptr()),
            )
        }
        .expect("open parent helper gate");
        assert_eq!(
            unsafe { WaitForSingleObject(gate, 10_000) },
            windows::Win32::Foundation::WAIT_OBJECT_0
        );
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(gate);
        }

        let stage_path = PathBuf::from(&ready).with_extension("stages");
        let diagnostic_path = PathBuf::from(&ready).with_extension("diagnostic");
        let record = |stage: &str| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stage_path)
                .unwrap();
            writeln!(file, "{stage}").unwrap();
        };
        let panic_path = stage_path.clone();
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&panic_path)
            {
                let _ = writeln!(file, "panic: {panic}");
            }
            hook(panic);
        }));

        let mut stdin = std::io::stdin().lock();
        let mut length = [0; 4];
        stdin.read_exact(&mut length).unwrap();
        let length = u32::from_le_bytes(length) as usize;
        assert!((23..=HELPER_MAX_FRAME).contains(&length));
        let mut frame = vec![0; length];
        stdin.read_exact(&mut frame).unwrap();
        let (request, request_id, revision) = decode_helper_request(&frame).unwrap();
        record(&format!("frame_read:{request_id}:{}", revision.0));
        let HelperRequest::Search(query) = request else {
            panic!("fixture expected a Search frame");
        };
        assert_eq!(query, std::env::var(BLOCKED_IPC_QUERY_ENV).unwrap());
        let instance = std::env::var(BLOCKED_IPC_INSTANCE_ENV).unwrap();
        let expected_class = format!("EVERYTHING_TASKBAR_NOTIFICATION({instance})");
        let selected = everything_ipc::IpcWindow::with_instance(Some(&instance))
            .expect("private fixture class must be present");
        assert_eq!(selected.class_name(), expected_class);
        record("private_window_selected");
        let client = everything_ipc::wm::EverythingClient::with_instance(Some(&instance))
            .expect("explicitly bind to the private instance");
        record("client_created");
        record("query_entered");
        let result = client
            .query_wait(&query)
            .request_flags(everything_ipc::wm::RequestFlags::FileName)
            .call();
        std::fs::write(&diagnostic_path, format!("query returned: {result:?}")).unwrap();
        panic!("private blocked query unexpectedly returned: {result:?}");
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_echo_fixture() {
        use std::io::{Read, Write};
        let ready = std::env::var(HELPER_TEST_ECHO_READY_ENV).unwrap();
        let expected_query = std::env::var(HELPER_TEST_ECHO_QUERY_ENV).unwrap();
        let result_path = PathBuf::from(std::env::var(HELPER_TEST_ECHO_PATH_ENV).unwrap());
        let mut stdin = std::io::stdin().lock();
        let mut length = [0; 4];
        stdin.read_exact(&mut length).unwrap();
        let mut request = vec![0; u32::from_le_bytes(length) as usize];
        stdin.read_exact(&mut request).unwrap();
        assert_eq!(&request[..4], b"EIPC");
        assert_eq!(request[6], 2, "expected a Search helper request");
        let request_id = u64::from_le_bytes(request[7..15].try_into().unwrap());
        let revision = u64::from_le_bytes(request[15..23].try_into().unwrap());
        let query_len = u32::from_le_bytes(request[23..27].try_into().unwrap()) as usize;
        assert_eq!(
            std::str::from_utf8(&request[27..27 + query_len]).unwrap(),
            expected_query
        );
        std::fs::write(&ready, format!("{request_id}:{revision}")).unwrap();

        let mut reply = b"EIPC".to_vec();
        reply.extend_from_slice(&HELPER_PROTOCOL_VERSION.to_le_bytes());
        reply.push(2);
        reply.extend_from_slice(&request_id.to_le_bytes());
        reply.extend_from_slice(&revision.to_le_bytes());
        reply.push(0);
        reply.extend_from_slice(&1u32.to_le_bytes());
        write_wide_path(&mut reply, &result_path).unwrap();
        write_wide_path(&mut reply, Path::new("echo-result.txt")).unwrap();
        reply.push(0);
        reply.push(1);
        reply.extend_from_slice(&42u64.to_le_bytes());
        decode_helper_reply(
            &reply,
            &HelperRequest::Search(expected_query),
            request_id,
            ProviderRevision(revision),
        )
        .expect("echo fixture produced a valid response frame");
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&(reply.len() as u32).to_le_bytes())
            .unwrap();
        stdout.write_all(&reply).unwrap();
        stdout.flush().unwrap();
        let release = std::env::var(HELPER_TEST_ECHO_RELEASE_ENV).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !Path::new(&release).exists() {
            assert!(Instant::now() < deadline, "echo helper release timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(windows)]
    const REAPER_ERROR_CHILD_ENV: &str = "GPUI_FILER_EVERYTHING_REAPER_ERROR_CHILD";

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_reaper_error_child_fixture() {
        let Ok(ready_path) = std::env::var(REAPER_ERROR_CHILD_ENV) else {
            return;
        };
        std::fs::write(ready_path, b"started").unwrap();
        thread::sleep(Duration::from_millis(250));
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_reaper_wait_error_fixture() {
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0},
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };

        let ready_path = std::env::var(HELPER_TEST_READY_ENV).unwrap();
        let child_exe = std::env::current_exe().unwrap();
        let mut command = Command::new(child_exe);
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("everything::tests::everything_ipc_reaper_error_child_fixture")
            .env(REAPER_ERROR_CHILD_ENV, &ready_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("start exact child fixture");
        let child_pid = child.id();
        let slots = Arc::new(async_lock::Semaphore::new(2));
        let slot = slots.try_acquire_arc().expect("fixture slot available");
        let second_slot = slots
            .try_acquire_arc()
            .expect("second fixture slot available");
        let waiting_thread = thread::spawn(|| thread::sleep(Duration::from_secs(10)));
        let reaper = thread::spawn(move || {
            reap_helper_resources(child, slot, vec![waiting_thread], |_| {
                Err(std::io::Error::other("injected wait failure"))
            });
        });
        reaper.join().unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !std::path::Path::new(&ready_path).exists() {
            assert!(
                Instant::now() < ready_deadline,
                "child fixture did not start"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(HELPER_DISABLED.load(Ordering::Acquire));
        assert!(
            slots.try_acquire_arc().is_none(),
            "wait failure released a permit"
        );

        let child_process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, child_pid) }
            .expect("open child synchronization handle");
        assert_eq!(
            unsafe { WaitForSingleObject(child_process, 5_000) },
            WAIT_OBJECT_0,
            "child did not exit after injected wait failure"
        );
        assert!(
            slots.try_acquire_arc().is_none(),
            "child exit released slot without a confirmed wait"
        );

        let result = supervise_helper_with_command(
            second_slot,
            HelperRequest::Search("disabled-test".into()),
            provider().revision(),
            Instant::now() + Duration::from_secs(1),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Command::new("cmd.exe"),
            false,
        );
        assert!(matches!(result, Err(WorkerError::Unavailable)));
        unsafe {
            let _ = CloseHandle(child_process);
        }
    }

    #[cfg(windows)]
    #[test]
    fn confirmed_reaper_wait_joins_threads_and_releases_slot() {
        let slots = Arc::new(async_lock::Semaphore::new(1));
        let slot = slots.try_acquire_arc().expect("test slot available");
        let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let joined_by_thread = joined.clone();
        let (release_worker, wait_for_release) = mpsc::channel();
        let worker = thread::spawn(move || {
            wait_for_release
                .recv_timeout(Duration::from_secs(5))
                .expect("test released gated worker");
            joined_by_thread.store(true, Ordering::Release);
        });
        let child = Command::new("cmd.exe")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("start short-lived helper fixture");
        let (wait_confirmed, confirmed_wait) = mpsc::channel();
        let reaper = thread::spawn(move || {
            reap_helper_resources(child, slot, vec![worker], |child| {
                let status = child.wait();
                wait_confirmed.send(()).unwrap();
                status
            });
        });
        confirmed_wait
            .recv_timeout(Duration::from_secs(5))
            .expect("child wait was not confirmed within the bound");
        assert!(
            slots.try_acquire_arc().is_none(),
            "slot was released before the gated worker could be joined"
        );
        release_worker.send(()).unwrap();
        reaper.join().unwrap();
        assert!(
            joined.load(Ordering::Acquire),
            "worker thread was not joined"
        );
        assert!(
            slots.try_acquire_arc().is_some(),
            "confirmed wait did not release slot"
        );
    }

    #[cfg(windows)]
    #[test]
    fn wait_error_retains_capacity_in_subprocess() {
        let temp = tempfile::tempdir().expect("fresh fixture directory");
        let ready_path = temp.path().join("reaper-error.ready");
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new(executable);
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("everything::tests::everything_ipc_reaper_wait_error_fixture")
            .env(HELPER_TEST_READY_ENV, &ready_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut fixture = command.spawn().expect("start isolated wait-error fixture");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = fixture.try_wait().unwrap() {
                assert!(status.success(), "wait-error fixture failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "wait-error fixture timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn everything_ipc_supervisor_parent_fixture() {
        let ready_path = std::env::var(HELPER_TEST_READY_ENV)
            .expect("parent fixture readiness path was not supplied");
        let slots = Arc::new(async_lock::Semaphore::new(1));
        let slot = slots.try_acquire_arc().expect("fixture slot available");
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new(executable);
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("everything::tests::everything_ipc_hold_open_fixture")
            .env(HELPER_TEST_READY_ENV, ready_path);
        let result = supervise_helper_with_deadline(
            slot,
            HelperRequest::Search("job-close-test".into()),
            provider().revision(),
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            command,
            false,
        );
        let _ = result;
        panic!("supervisor returned before parent fixture was terminated");
    }

    #[cfg(windows)]
    #[test]
    fn parent_exit_closes_job_and_terminates_exact_test_helper() {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                PROCESS_TERMINATE, WaitForSingleObject,
            },
        };

        struct Cleanup {
            parent: std::process::Child,
            nested: windows::Win32::Foundation::HANDLE,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                use std::os::windows::io::AsRawHandle;
                use windows::Win32::{
                    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
                    System::Threading::WaitForSingleObject,
                };
                if let Ok(None) = self.parent.try_wait() {
                    let _ = self.parent.kill();
                    let process = HANDLE(self.parent.as_raw_handle() as _);
                    if unsafe { WaitForSingleObject(process, 5_000) } == WAIT_OBJECT_0 {
                        let _ = self.parent.try_wait();
                    }
                }
                if !self.nested.is_invalid() {
                    unsafe {
                        if WaitForSingleObject(self.nested, 0)
                            == windows::Win32::Foundation::WAIT_TIMEOUT
                        {
                            let _ =
                                windows::Win32::System::Threading::TerminateProcess(self.nested, 1);
                            let _ = WaitForSingleObject(self.nested, 5_000);
                        }
                        let _ = CloseHandle(self.nested);
                    }
                }
            }
        }

        let temp = tempfile::tempdir().expect("fresh fixture directory");
        let nested_ready = temp.path().join("nested.ready");
        let executable = std::env::current_exe().unwrap();
        let mut parent = Command::new(executable);
        parent
            .arg("--ignored")
            .arg("--exact")
            .arg("everything::tests::everything_ipc_supervisor_parent_fixture")
            .env(HELPER_TEST_READY_ENV, &nested_ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = parent.spawn().expect("start parent fixture");
        let parent_pid = child.id();
        let mut cleanup = Cleanup {
            parent: child,
            nested: windows::Win32::Foundation::HANDLE::default(),
        };

        let ready_deadline = Instant::now() + Duration::from_secs(15);
        let nested_pid = loop {
            if let Some(pid) = std::fs::read_to_string(&nested_ready)
                .ok()
                .and_then(|contents| contents.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < ready_deadline,
                "nested helper did not publish its pid"
            );
            assert!(
                cleanup.parent.try_wait().unwrap().is_none(),
                "parent fixture exited before readiness"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_ne!(
            nested_pid, parent_pid,
            "nested helper pid must identify a distinct process"
        );
        cleanup.nested = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                nested_pid,
            )
        }
        .unwrap_or_else(|error| panic!("could not open nested helper pid {nested_pid}: {error}"));
        assert_eq!(
            unsafe { WaitForSingleObject(cleanup.nested, 0) },
            WAIT_TIMEOUT
        );
        assert!(
            cleanup.parent.try_wait().unwrap().is_none(),
            "parent was not alive before termination"
        );

        cleanup
            .parent
            .kill()
            .expect("terminate exact parent process");
        let parent_process =
            windows::Win32::Foundation::HANDLE(cleanup.parent.as_raw_handle() as _);
        assert_eq!(
            unsafe { WaitForSingleObject(parent_process, 5_000) },
            WAIT_OBJECT_0,
            "parent process did not exit within the bounded wait"
        );
        assert!(
            cleanup.parent.try_wait().unwrap().is_some(),
            "parent process signal did not reap the exact child"
        );
        assert_eq!(
            unsafe { WaitForSingleObject(cleanup.nested, 10_000) },
            WAIT_OBJECT_0,
            "nested helper remained alive after its supervisor process exited"
        );
        unsafe {
            let _ = CloseHandle(cleanup.nested);
        }
        cleanup.nested = windows::Win32::Foundation::HANDLE::default();
        drop(cleanup);
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_supervisor_kills_and_reaps_exact_test_helper() {
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0},
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };

        let ready_path = std::env::temp_dir().join(format!(
            "gpui-filer-everything-helper-{}-{}.ready",
            std::process::id(),
            next_helper_id()
        ));
        let slots = Arc::new(async_lock::Semaphore::new(1));
        let slot = slots.try_acquire_arc().expect("test slot available");
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let revision = provider().revision();
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new(executable);
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("everything::tests::everything_ipc_hold_open_fixture")
            .env(HELPER_TEST_READY_ENV, &ready_path);
        let child_cancelled = cancelled.clone();
        let supervisor = thread::spawn(move || {
            supervise_helper_with_command(
                slot,
                HelperRequest::Search("test".into()),
                revision,
                Instant::now() + Duration::from_secs(10),
                child_cancelled,
                command,
                false,
            )
        });
        struct CancelWhenDropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for CancelWhenDropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _cancel_guard = CancelWhenDropped(cancelled.clone());

        let ready_deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&ready_path)
                .ok()
                .and_then(|contents| contents.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < ready_deadline,
                "fixture helper did not publish its process id"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .unwrap_or_else(|error| panic!("could not open fixture pid {pid}: {error}"));
        assert_eq!(
            unsafe { WaitForSingleObject(process, 0) },
            windows::Win32::Foundation::WAIT_TIMEOUT
        );
        assert!(
            slots.try_acquire_arc().is_none(),
            "slot released while helper is alive"
        );

        cancelled.store(true, Ordering::Release);
        let result = supervisor.join().unwrap();
        assert!(
            matches!(&result, Err(WorkerError::Cancelled)),
            "unexpected supervisor error: {:?}",
            result.err()
        );
        assert_eq!(
            unsafe { WaitForSingleObject(process, 5_000) },
            WAIT_OBJECT_0
        );
        assert!(
            slots.try_acquire_arc().is_some(),
            "slot not released after confirmed exit"
        );
        unsafe {
            let _ = CloseHandle(process);
        }
        let _ = std::fs::remove_file(ready_path);
    }

    #[cfg(windows)]
    #[test]
    fn timed_out_supervisor_kills_and_reaps_exact_test_helper() {
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };

        let ready_path = std::env::temp_dir().join(format!(
            "gpui-filer-everything-helper-{}-{}.ready",
            std::process::id(),
            next_helper_id()
        ));
        let slots = Arc::new(async_lock::Semaphore::new(1));
        let slot = slots.try_acquire_arc().expect("test slot available");
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let revision = provider().revision();
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new(executable);
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("everything::tests::everything_ipc_hold_open_fixture")
            .env(HELPER_TEST_READY_ENV, &ready_path);
        let child_cancelled = cancelled.clone();
        let supervisor = thread::spawn(move || {
            supervise_helper_with_command(
                slot,
                HelperRequest::Search("test".into()),
                revision,
                Instant::now() + Duration::from_secs(15),
                child_cancelled,
                command,
                false,
            )
        });

        // Deadline はfixture起動の余裕を含め、ready待ちの期限より後に置く。
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&ready_path)
                .ok()
                .and_then(|contents| contents.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < ready_deadline,
                "fixture helper did not publish its process id"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .unwrap_or_else(|error| panic!("could not open fixture pid {pid}: {error}"));
        assert_eq!(unsafe { WaitForSingleObject(process, 0) }, WAIT_TIMEOUT);
        assert!(
            slots.try_acquire_arc().is_none(),
            "slot released while helper is alive"
        );

        let result = supervisor.join().unwrap();
        assert!(matches!(result, Err(WorkerError::Timeout)));
        assert!(!cancelled.load(Ordering::Acquire));
        assert_eq!(
            unsafe { WaitForSingleObject(process, 5_000) },
            WAIT_OBJECT_0
        );
        assert!(
            slots.try_acquire_arc().is_some(),
            "slot was not released after helper exit"
        );
        unsafe {
            let _ = CloseHandle(process);
        }
        let _ = std::fs::remove_file(ready_path);
    }

    #[test]
    fn queued_worker_timeout_survives_elapsed_caller_deadline() {
        let result: Option<Result<(), ProviderTerminal>> =
            resolve_worker_poll(Some(Err(WorkerError::Timeout)), false, false, true);
        assert_eq!(result, Some(Err(ProviderTerminal::Timeout)));
    }

    #[test]
    fn empty_worker_channel_uses_elapsed_caller_deadline_as_timeout() {
        let result: Option<Result<(), ProviderTerminal>> =
            resolve_worker_poll(None, false, false, true);
        assert_eq!(result, Some(Err(ProviderTerminal::Timeout)));
    }

    #[test]
    fn worker_cancellation_remains_cancelled_without_a_terminal_message() {
        let result: Option<Result<(), ProviderTerminal>> =
            resolve_worker_poll(None, false, true, true);
        assert_eq!(result, Some(Err(ProviderTerminal::Cancelled)));
    }

    #[cfg(all(windows, feature = "everything-private-ipc-test"))]
    #[gpui::test]
    async fn production_search_reaps_helper_blocked_in_private_everything_copydata(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                PROCESS_TERMINATE, SetEvent, TerminateProcess, WaitForSingleObject,
            },
        };
        use windows::core::PCWSTR;

        struct FakeServer {
            child: std::process::Child,
            entered: HANDLE,
            release: HANDLE,
            ready: PathBuf,
        }
        impl Drop for FakeServer {
            fn drop(&mut self) {
                unsafe {
                    let _ = SetEvent(self.release);
                    let process = HANDLE(self.child.as_raw_handle() as _);
                    if WaitForSingleObject(process, 4_000) == WAIT_TIMEOUT {
                        let _ = self.child.kill();
                        let _ = WaitForSingleObject(process, 4_000);
                    }
                    let _ = self.child.try_wait();
                    let _ = CloseHandle(self.entered);
                    let _ = CloseHandle(self.release);
                }
                let _ = std::fs::remove_file(&self.ready);
            }
        }
        struct HelperProcess(HANDLE);
        impl Drop for HelperProcess {
            fn drop(&mut self) {
                unsafe {
                    if WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT {
                        let _ = TerminateProcess(self.0, 1);
                        let _ = WaitForSingleObject(self.0, 5_000);
                    }
                    let _ = CloseHandle(self.0);
                }
            }
        }
        struct PreflightChild(Option<std::process::Child>);
        impl Drop for PreflightChild {
            fn drop(&mut self) {
                let Some(child) = self.0.as_mut() else {
                    return;
                };
                if child.try_wait().ok().flatten().is_some() {
                    self.0.take();
                    return;
                }

                let _ = child.kill();
                let process = HANDLE(child.as_raw_handle() as _);
                if unsafe { WaitForSingleObject(process, 250) } == WAIT_OBJECT_0 {
                    if let Some(child) = self.0.as_mut() {
                        let _ = child.try_wait();
                    }
                    self.0.take();
                    return;
                }

                // Drop runs on the GPUI test executor; keep any delayed process
                // reap off that executor while retaining ownership in the reaper.
                if let Some(mut child) = self.0.take() {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
        }
        struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        async fn run_preflight(
            executor: &gpui::BackgroundExecutor,
            executable: &Path,
            receipt: &Path,
        ) -> std::process::ExitStatus {
            let mut child = PreflightChild(Some(
                Command::new(executable)
                    .arg(HELPER_SWITCH)
                    .env(PRIVATE_IPC_TEST_PREFLIGHT_ENV, receipt)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("start helper-mode preflight child"),
            ));
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(status) = child
                    .0
                    .as_mut()
                    .expect("preflight child remains owned")
                    .try_wait()
                    .expect("poll preflight child")
                {
                    child.0.take();
                    return status;
                }
                assert!(Instant::now() < deadline, "helper-mode preflight timed out");
                executor.timer(Duration::from_millis(5)).await;
            }
        }

        let executor = cx.background_executor.clone();
        let test_executable = std::env::current_exe().expect("unit-test executable");
        let app_executable = test_executable
            .parent()
            .and_then(Path::parent)
            .expect("unit-test executable must be under target/<triple>/debug/deps")
            .join("gpui-filer-v6.exe");
        assert!(
            app_executable.is_file(),
            "build the private-test app binary in the same target directory: {}",
            app_executable.display()
        );
        let default_app_executable =
            std::env::var_os("GPUI_FILER_EVERYTHING_DEFAULT_TEST_EXECUTABLE")
                .map(PathBuf::from)
                .expect("set the default-feature app executable for the negative preflight case");
        assert!(
            default_app_executable.is_file(),
            "default-feature app executable must exist: {}",
            default_app_executable.display()
        );

        for cancel_caller in [true, false] {
            let fixture_id = next_helper_id();
            let temp = tempfile::tempdir().expect("private fixture directory");
            let unique_id = format!("{}-{fixture_id}", std::process::id());
            let instance = format!("gpui-filer-{unique_id}");
            let query = format!("blocked-send-{fixture_id}");
            let entered_name = format!("Local\\gpui-filer-entered-{unique_id}");
            let release_name = format!("Local\\gpui-filer-release-{unique_id}");
            let entered_wide = entered_name.encode_utf16().chain([0]).collect::<Vec<_>>();
            let release_wide = release_name.encode_utf16().chain([0]).collect::<Vec<_>>();
            let entered = unsafe { CreateEventW(None, true, false, PCWSTR(entered_wide.as_ptr())) }
                .expect("create entered event");
            let release = unsafe { CreateEventW(None, true, false, PCWSTR(release_wide.as_ptr())) }
                .expect("create release event");

            let server_ready = temp.path().join("server.ready");
            let server_diagnostic = temp.path().join("server.diagnostic");
            let fixture_executable = std::env::current_exe().expect("test executable");
            let mut server_command = Command::new(fixture_executable);
            server_command
                .arg("--ignored")
                .arg("--exact")
                .arg("everything::tests::everything_ipc_private_blocked_server_fixture")
                .env(BLOCKED_IPC_INSTANCE_ENV, &instance)
                .env(BLOCKED_IPC_ENTERED_ENV, &entered_name)
                .env(BLOCKED_IPC_RELEASE_ENV, &release_name)
                .env(BLOCKED_IPC_QUERY_ENV, &query)
                .env(BLOCKED_IPC_SERVER_READY_ENV, &server_ready)
                .env(BLOCKED_IPC_SERVER_DIAGNOSTIC_ENV, &server_diagnostic)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let server_child = server_command.spawn().expect("start private fake server");
            let mut server = FakeServer {
                child: server_child,
                entered,
                release,
                ready: server_ready.clone(),
            };

            let server_deadline = Instant::now() + Duration::from_secs(10);
            let expected_class = format!("EVERYTHING_TASKBAR_NOTIFICATION({instance})");
            loop {
                if std::fs::read_to_string(&server_ready).ok().as_deref()
                    == Some(expected_class.as_str())
                {
                    let class_name = expected_class.as_str();
                    assert_ne!(class_name, "EVERYTHING_TASKBAR_NOTIFICATION");
                    assert_ne!(class_name, "EVERYTHING_TASKBAR_NOTIFICATION(1.5a)");
                    break;
                }
                assert!(
                    Instant::now() < server_deadline,
                    "private server did not start"
                );
                assert!(
                    server
                        .child
                        .try_wait()
                        .expect("poll private server")
                        .is_none()
                );
                executor.timer(Duration::from_millis(5)).await;
            }

            if cancel_caller {
                let receipt = temp.path().join("preflight.receipt");
                let feature_status = run_preflight(&executor, &app_executable, &receipt).await;
                assert!(feature_status.success(), "feature helper preflight failed");
                assert_eq!(
                    std::fs::read_to_string(&receipt).expect("feature preflight receipt"),
                    PRIVATE_IPC_TEST_PREFLIGHT_RECEIPT
                );
                assert_eq!(
                    unsafe { WaitForSingleObject(server.entered, 0) },
                    WAIT_TIMEOUT,
                    "feature preflight sent an IPC request"
                );
                std::fs::remove_file(&receipt).expect("remove positive preflight receipt");

                let default_status =
                    run_preflight(&executor, &default_app_executable, &receipt).await;
                assert!(
                    !default_status.success(),
                    "default-feature helper unexpectedly accepted private preflight"
                );
                assert!(
                    !receipt.exists(),
                    "default-feature helper emitted a private preflight receipt"
                );
                assert_eq!(
                    unsafe { WaitForSingleObject(server.entered, 0) },
                    WAIT_TIMEOUT,
                    "default-feature preflight sent WM_COPYDATA"
                );
            }

            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let _cancel_on_drop = CancelOnDrop(cancelled.clone());
            let spare_slot = provider()
                .slots
                .try_acquire_arc()
                .expect("one provider capacity slot remains available");
            let revision = provider().revision();
            let helper_pid_path = temp.path().join("production-helper.pid");
            let env_instance = instance.clone();
            let env_pid_path = helper_pid_path.clone();
            let executable = app_executable.clone();
            let (terminal_tx, terminal_rx) = mpsc::channel();
            let command = TestSearchCommand {
                factory: Arc::new(move || {
                    let mut command = Command::new(&executable);
                    command
                        .arg(HELPER_SWITCH)
                        .env(PRIVATE_IPC_TEST_INSTANCE_ENV, &env_instance)
                        .env(PRIVATE_IPC_TEST_HELPER_PID_ENV, &env_pid_path)
                        .stdin(Stdio::piped());
                    command
                }),
                expect_response: true,
                captured_reply: Arc::new(Mutex::new(None)),
                terminal_observer: Some(Arc::new(move |terminal, observed_revision| {
                    let _ = terminal_tx.send((terminal, observed_revision));
                })),
            };
            let caller_cancelled = cancelled.clone();
            let caller_executor = executor.clone();
            let query_for_search = query.clone();
            let request_started = Instant::now();
            let search = executor.spawn(async move {
                search_with_test_command(
                    caller_executor,
                    query_for_search,
                    caller_cancelled,
                    revision,
                    command,
                )
                .await
            });
            let (outcome_tx, outcome_rx) = mpsc::channel();
            let _outcome_collector = executor.spawn(async move {
                let _ = outcome_tx.send(search.await);
            });

            let entry_deadline = Instant::now() + Duration::from_secs(10);
            let helper = loop {
                if let Some(pid) = std::fs::read_to_string(&helper_pid_path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                {
                    let handle = unsafe {
                        OpenProcess(
                            PROCESS_SYNCHRONIZE
                                | PROCESS_QUERY_LIMITED_INFORMATION
                                | PROCESS_TERMINATE,
                            false,
                            pid,
                        )
                    }
                    .unwrap_or_else(|error| panic!("open exact production helper {pid}: {error}"));
                    let process = HelperProcess(handle);
                    assert_eq!(unsafe { WaitForSingleObject(process.0, 0) }, WAIT_TIMEOUT);
                    if unsafe { WaitForSingleObject(server.entered, 0) } == WAIT_OBJECT_0 {
                        break process;
                    }
                    if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
                        panic!(
                            "production helper exited before endpoint entry; server={}",
                            std::fs::read_to_string(&server_diagnostic).unwrap_or_default()
                        );
                    }
                    // The helper PID is published only once; continue waiting for WM_COPYDATA.
                    loop {
                        if unsafe { WaitForSingleObject(server.entered, 0) } == WAIT_OBJECT_0 {
                            break;
                        }
                        assert!(
                            Instant::now() < entry_deadline,
                            "endpoint did not receive query"
                        );
                        assert_eq!(unsafe { WaitForSingleObject(process.0, 0) }, WAIT_TIMEOUT);
                        executor.timer(Duration::from_millis(5)).await;
                    }
                    break process;
                }
                assert!(
                    Instant::now() < entry_deadline,
                    "production helper did not start"
                );
                assert!(
                    terminal_rx.try_recv().is_err(),
                    "caller ended before endpoint entry"
                );
                executor.timer(Duration::from_millis(5)).await;
            };

            assert_eq!(unsafe { WaitForSingleObject(helper.0, 0) }, WAIT_TIMEOUT);
            assert!(provider().slots.try_acquire_arc().is_none());
            assert!(
                terminal_rx.try_recv().is_err(),
                "caller was not pending at endpoint entry"
            );
            let terminal_triggered = Instant::now();
            if cancel_caller {
                cancelled.store(true, Ordering::Release);
            }
            let terminal_deadline = if cancel_caller {
                terminal_triggered + Duration::from_secs(2)
            } else {
                request_started + FOLDER_SIZE_DEADLINE + Duration::from_secs(2)
            };
            let outcome = loop {
                match outcome_rx.try_recv() {
                    Ok(outcome) => break outcome,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("search outcome collector disconnected")
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                assert!(
                    Instant::now() < terminal_deadline,
                    "caller did not reach its {:?} terminal within the bound",
                    if cancel_caller {
                        ProviderTerminal::Cancelled
                    } else {
                        ProviderTerminal::Timeout
                    }
                );
                executor.timer(Duration::from_millis(5)).await;
            };
            let terminal_elapsed = Instant::now().saturating_duration_since(terminal_triggered);
            if cancel_caller {
                assert!(terminal_elapsed < Duration::from_secs(2));
            } else {
                assert!(
                    request_started.elapsed() < FOLDER_SIZE_DEADLINE + Duration::from_secs(2),
                    "caller timeout exceeded deadline plus scheduling allowance"
                );
            }
            assert_eq!(
                outcome.terminal,
                if cancel_caller {
                    ProviderTerminal::Cancelled
                } else {
                    ProviderTerminal::Timeout
                }
            );
            assert!(outcome.results.is_empty());
            if cancel_caller {
                assert_eq!(outcome.revision, revision);
                assert_eq!(provider().revision(), revision);
            } else {
                assert_ne!(outcome.revision, revision);
                assert_eq!(provider().revision(), outcome.revision);
            }
            assert_eq!(
                terminal_rx.try_recv().expect("terminal observer ran"),
                (outcome.terminal, outcome.revision)
            );
            assert_eq!(
                unsafe { WaitForSingleObject(helper.0, 5_000) },
                WAIT_OBJECT_0,
                "supervisor did not terminate the exact production helper"
            );
            assert!(
                server
                    .child
                    .try_wait()
                    .expect("poll held private server")
                    .is_none()
            );

            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(reusable_slot) = provider().slots.try_acquire_arc() {
                    drop(reusable_slot);
                    break;
                }
                assert!(
                    Instant::now() < cleanup_deadline,
                    "provider slot was not reusable after helper and response-reader join"
                );
                executor.timer(Duration::from_millis(5)).await;
            }
            drop(helper);
            drop(spare_slot);

            unsafe { SetEvent(server.release).expect("release private server WndProc") };
            assert_eq!(
                unsafe { WaitForSingleObject(HANDLE(server.child.as_raw_handle() as _), 5_000) },
                WAIT_OBJECT_0,
                "private server did not exit after release"
            );
            let _ = server.child.try_wait();
        }
    }

    #[cfg(windows)]
    #[gpui::test]
    async fn search_caller_timeout_and_cancellation_reap_held_response_helpers(
        cx: &mut gpui::TestAppContext,
    ) {
        use windows::Win32::{
            Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        };

        async fn run_case(
            executor: gpui::BackgroundExecutor,
            cancelled_after_ready: bool,
            expected: ProviderTerminal,
        ) {
            let ready_path = std::env::temp_dir().join(format!(
                "gpui-filer-everything-caller-{}-{}.ready",
                std::process::id(),
                next_helper_id()
            ));
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let revision = provider().revision();
            // 空きslotを一つ保持し、helper使用中の残りslotの占有を検査する。
            let spare_slot = provider()
                .slots
                .try_acquire_arc()
                .expect("one spare provider slot is available");
            let command_ready_path = ready_path.clone();
            let command = TestSearchCommand {
                factory: Arc::new(move || {
                    let script = concat!(
                        "$s=[Console]::OpenStandardInput();",
                        "$b=New-Object byte[] 4;$o=0;",
                        "while($o -lt 4){$n=$s.Read($b,$o,4-$o);if($n -le 0){exit 2};$o+=$n};",
                        "$l=[BitConverter]::ToUInt32($b,0);",
                        "$r=New-Object byte[] ([int]$l);$o=0;",
                        "while($o -lt $r.Length){$n=$s.Read($r,$o,$r.Length-$o);if($n -le 0){exit 3};$o+=$n};",
                        "[IO.File]::WriteAllText($env:GPUI_FILER_EVERYTHING_HELPER_TEST_READY,[string]$PID);",
                        "Start-Sleep -Seconds 60"
                    );
                    let mut command = Command::new("powershell.exe");
                    command
                        .args([
                            "-NoLogo",
                            "-NoProfile",
                            "-NonInteractive",
                            "-Command",
                            script,
                        ])
                        .env(HELPER_TEST_READY_ENV, &command_ready_path);
                    command
                }),
                expect_response: true,
                captured_reply: Arc::new(Mutex::new(None)),
                terminal_observer: None,
            };
            let caller_cancelled = cancelled.clone();
            let caller_executor = executor.clone();
            let started = Instant::now();
            let search = executor.spawn(async move {
                search_with_test_command(
                    caller_executor,
                    "held-response-test".into(),
                    caller_cancelled,
                    revision,
                    command,
                )
                .await
            });

            let ready_deadline = Instant::now() + Duration::from_secs(5);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&ready_path)
                    .ok()
                    .and_then(|contents| contents.parse::<u32>().ok())
                {
                    break pid;
                }
                assert!(
                    Instant::now() < ready_deadline,
                    "PowerShell helper did not consume the serialized request"
                );
                executor.timer(Duration::from_millis(5)).await;
            };
            let process = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    false,
                    pid,
                )
            }
            .unwrap_or_else(|error| panic!("could not open PowerShell helper {pid}: {error}"));
            assert_eq!(unsafe { WaitForSingleObject(process, 0) }, WAIT_TIMEOUT);
            assert!(
                provider().slots.try_acquire_arc().is_none(),
                "provider capacity released while helper is alive"
            );

            if cancelled_after_ready {
                cancelled.store(true, Ordering::Release);
            }
            let outcome = search.await;
            let caller_bound = if expected == ProviderTerminal::Timeout {
                FOLDER_SIZE_DEADLINE + Duration::from_secs(2)
            } else {
                Duration::from_secs(2)
            };
            assert!(
                started.elapsed() < caller_bound,
                "caller exceeded its {expected:?} bound"
            );
            assert_eq!(outcome.terminal, expected);
            assert_eq!(
                unsafe { WaitForSingleObject(process, 5_000) },
                WAIT_OBJECT_0,
                "PowerShell helper process was not terminated"
            );
            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            let reusable_slot = loop {
                if let Some(slot) = provider().slots.try_acquire_arc() {
                    break slot;
                }
                assert!(
                    Instant::now() < cleanup_deadline,
                    "provider capacity was not reusable after helper exit"
                );
                executor.timer(Duration::from_millis(5)).await;
            };
            drop(reusable_slot);
            drop(spare_slot);
            unsafe { CloseHandle(process) }.unwrap();
            let _ = std::fs::remove_file(ready_path);
        }

        let executor = cx.background_executor.clone();
        run_case(executor.clone(), false, ProviderTerminal::Timeout).await;
        run_case(executor, true, ProviderTerminal::Cancelled).await;
    }

    #[test]
    fn revision_invalidation_is_compare_and_advance() {
        let before = ProviderRevision::current();
        let after = invalidate(before);
        assert_ne!(before, after);
        assert_eq!(invalidate(before), after);
    }
    #[test]
    fn cache_evicts_oldest_entry_and_honors_bypass() {
        let provider = Provider {
            revision: AtomicU64::new(1),
            cache_generation: AtomicU64::new(1),
            next_folder_size_request_order: AtomicU64::new(1),
            slots: Arc::new(async_lock::Semaphore::new(PROVIDER_SLOTS)),
            cache: Mutex::new(VecDeque::with_capacity(FOLDER_CACHE_CAPACITY)),
            active_probes: Mutex::new(HashMap::new()),
        };
        let identity = EntrySourceIdentity::test_folder();
        let revision = provider.revision();
        let mut paths = Vec::new();
        for index in 0..=FOLDER_CACHE_CAPACITY {
            let path = PathBuf::from(format!(r"C:\cache\{index}"));
            let result = FolderSizeResult {
                key: EntryKey::path(path.clone(), EntryKind::Folder),
                path: path.clone(),
                fact: FolderSizeFact {
                    bytes: index as u64,
                    provenance: FolderSizeProvenance::EverythingIndex,
                },
                revision,
                cache_generation: FolderSizeCacheGeneration(1),
                freshness: FolderSizeFreshness::new(),
            };
            provider.insert(
                &result.path,
                result.fact,
                identity.clone(),
                result.revision,
                &result.freshness,
                index as u64 + 1,
                &std::sync::atomic::AtomicBool::new(false),
            );
            paths.push(path);
        }

        assert_eq!(provider.cache.lock().unwrap().len(), FOLDER_CACHE_CAPACITY);
        assert!(
            provider
                .cached(&paths[0], &identity, revision, true,)
                .is_none()
        );
        assert!(
            provider
                .cached(&paths[FOLDER_CACHE_CAPACITY], &identity, revision, true,)
                .is_some()
        );
        assert!(
            provider
                .cached(&paths[1], &identity, revision, false,)
                .is_none()
        );
    }

    #[test]
    fn cached_folder_size_requires_matching_source_identity() {
        let provider = Provider {
            revision: AtomicU64::new(1),
            cache_generation: AtomicU64::new(1),
            next_folder_size_request_order: AtomicU64::new(1),
            slots: Arc::new(async_lock::Semaphore::new(PROVIDER_SLOTS)),
            cache: Mutex::new(VecDeque::with_capacity(FOLDER_CACHE_CAPACITY)),
            active_probes: Mutex::new(HashMap::new()),
        };
        let identity_a = EntrySourceIdentity::test(EntryKind::Folder, "identity-a");
        let identity_b = EntrySourceIdentity::test(EntryKind::Folder, "identity-b");
        assert_ne!(identity_a, identity_b);

        let path = PathBuf::from(r"C:\cache\reused");
        let equivalent_path = PathBuf::from("c:/CACHE/reused");
        let revision = provider.revision();
        let other_revision = ProviderRevision(revision.0 + 1);
        let fact = FolderSizeFact {
            bytes: 42,
            provenance: FolderSizeProvenance::EverythingIndex,
        };
        provider.insert(
            &path,
            fact,
            identity_a.clone(),
            revision,
            &FolderSizeFreshness::new(),
            1,
            &std::sync::atomic::AtomicBool::new(false),
        );

        assert_eq!(
            provider.cached(&path, &identity_a, revision, true),
            Some(fact)
        );
        assert_eq!(
            provider.cached(&equivalent_path, &identity_a, revision, true),
            Some(fact)
        );
        assert_eq!(
            provider.cached(&equivalent_path, &identity_a, other_revision, true),
            None
        );
        assert_eq!(provider.cached(&path, &identity_b, revision, true), None);
        assert_eq!(
            provider.cached(&equivalent_path, &identity_a, revision, true),
            Some(fact)
        );
    }

    #[test]
    fn stale_revision_cannot_repopulate_cache_after_invalidation() {
        let provider = Provider {
            revision: AtomicU64::new(1),
            cache_generation: AtomicU64::new(1),
            next_folder_size_request_order: AtomicU64::new(1),
            slots: Arc::new(async_lock::Semaphore::new(PROVIDER_SLOTS)),
            cache: Mutex::new(VecDeque::with_capacity(FOLDER_CACHE_CAPACITY)),
            active_probes: Mutex::new(HashMap::new()),
        };
        let identity = EntrySourceIdentity::test_folder();
        let revision = provider.revision();
        let path = PathBuf::from(r"C:\cache\stale");
        let result = FolderSizeResult {
            key: EntryKey::path(path.clone(), EntryKind::Folder),
            path,
            fact: FolderSizeFact {
                bytes: 1,
                provenance: FolderSizeProvenance::EverythingIndex,
            },
            revision,
            cache_generation: FolderSizeCacheGeneration(1),
            freshness: FolderSizeFreshness::new(),
        };

        assert_ne!(provider.invalidate(revision), revision);
        provider.insert(
            &result.path,
            result.fact,
            identity,
            result.revision,
            &result.freshness,
            1,
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(provider.cache.lock().unwrap().is_empty());
    }

    #[test]
    fn folder_size_batches_cover_513_probes_with_protocol_bound() {
        let mut pending = 0..513;
        let mut observed = Vec::new();
        let mut counts = Vec::new();
        loop {
            let batch = take_folder_size_batch(&mut pending);
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= HELPER_MAX_FOLDER_PATHS);
            counts.push(batch.len());
            observed.extend(batch);
        }
        assert_eq!(counts, [512, 1]);
        assert_eq!(observed, (0..513).collect::<Vec<_>>());
    }

    fn folder_size_probes(
        root: &Path,
        count: usize,
    ) -> (Vec<FolderSizeProbe>, Arc<FolderSizeCacheLease>) {
        let (cache_generation, lease) = begin_folder_size_cache_lease();
        let probes = (0..count)
            .map(|index| {
                let path = root.join(format!("folder-{index}"));
                std::fs::create_dir(&path).expect("create folder-size probe directory");
                FolderSizeProbe {
                    key: EntryKey::path(path.clone(), EntryKind::Folder),
                    source_identity: EntrySourceIdentity::from_path(&path, EntryKind::Folder)
                        .expect("capture folder identity"),
                    freshness: lease.register(&path),
                    path,
                    cache_generation,
                }
            })
            .collect();
        (probes, lease)
    }

    #[gpui::test]
    async fn folder_size_query_completes_513_folders_over_bounded_helper_batches(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().expect("folder-size query fixture");
        let (probes, _lease) = folder_size_probes(temp.path(), 513);
        let revision = ProviderRevision::current();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let batch_sizes = Arc::new(Mutex::new(Vec::new()));
        let observed_batches = batch_sizes.clone();
        let runner: FolderSizeBatchRunner = Arc::new(move |slot, request, _, _, _| {
            drop(slot);
            let HelperRequest::FolderSizes(paths) = request else {
                return Err(WorkerError::Unavailable);
            };
            observed_batches
                .lock()
                .expect("batch observations")
                .push(paths.len());
            Ok(HelperReply::FolderSizes(vec![Some(42); paths.len()]))
        });

        let executor = cx.background_executor.clone();
        let query_executor = executor.clone();
        let outcome = executor
            .spawn(async move {
                query_folder_sizes_with_test_batch_runner(
                    query_executor,
                    probes,
                    cancelled.as_ref(),
                    revision,
                    true,
                    runner,
                )
                .await
            })
            .await;

        assert_eq!(*batch_sizes.lock().unwrap(), [512, 1]);
        assert_eq!(outcome.terminal, ProviderTerminal::Completed);
        assert_eq!(outcome.results.len(), 513);
        assert!(outcome.results.iter().all(|result| result.fact.bytes == 42));
    }

    #[gpui::test]
    async fn folder_size_query_stops_before_next_batch_after_cancellation(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().expect("folder-size cancellation fixture");
        let (probes, _lease) = folder_size_probes(temp.path(), 513);
        let revision = ProviderRevision::current();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let batch_sizes = Arc::new(Mutex::new(Vec::new()));
        let observed_batches = batch_sizes.clone();
        let runner: FolderSizeBatchRunner = Arc::new(move |slot, request, _, _, _| {
            drop(slot);
            let HelperRequest::FolderSizes(paths) = request else {
                return Err(WorkerError::Unavailable);
            };
            observed_batches
                .lock()
                .expect("batch observations")
                .push(paths.len());
            worker_cancelled.store(true, Ordering::Release);
            Ok(HelperReply::FolderSizes(vec![Some(42); paths.len()]))
        });

        let executor = cx.background_executor.clone();
        let query_executor = executor.clone();
        let outcome = executor
            .spawn(async move {
                query_folder_sizes_with_test_batch_runner(
                    query_executor,
                    probes,
                    cancelled.as_ref(),
                    revision,
                    true,
                    runner,
                )
                .await
            })
            .await;

        assert_eq!(*batch_sizes.lock().unwrap(), [512]);
        assert_eq!(outcome.terminal, ProviderTerminal::Cancelled);
        assert!(outcome.results.is_empty());
    }

    #[gpui::test]
    async fn folder_size_query_stops_before_next_batch_after_revision_change(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().expect("folder-size revision fixture");
        let (probes, _lease) = folder_size_probes(temp.path(), 513);
        let revision = ProviderRevision::current();
        let batch_sizes = Arc::new(Mutex::new(Vec::new()));
        let observed_batches = batch_sizes.clone();
        let runner: FolderSizeBatchRunner = Arc::new(move |slot, request, revision, _, _| {
            drop(slot);
            let HelperRequest::FolderSizes(paths) = request else {
                return Err(WorkerError::Unavailable);
            };
            observed_batches
                .lock()
                .expect("batch observations")
                .push(paths.len());
            provider().invalidate(revision);
            Ok(HelperReply::FolderSizes(vec![Some(42); paths.len()]))
        });

        let executor = cx.background_executor.clone();
        let query_executor = executor.clone();
        let outcome = executor
            .spawn(async move {
                query_folder_sizes_with_test_batch_runner(
                    query_executor,
                    probes,
                    &std::sync::atomic::AtomicBool::new(false),
                    revision,
                    true,
                    runner,
                )
                .await
            })
            .await;

        assert_eq!(*batch_sizes.lock().unwrap(), [512]);
        assert_eq!(outcome.terminal, ProviderTerminal::Completed);
        assert!(outcome.retry_invalidated);
        assert!(outcome.results.is_empty());
        assert_ne!(ProviderRevision::current(), revision);
    }

    #[test]
    fn malformed_search_paths_are_rejected_without_poisoning_valid_results() {
        let parent = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(sanitize_search_path(&parent, "found.txt").is_some());
        assert!(sanitize_search_path("relative", "found.txt").is_none());
        assert!(sanitize_search_path(&parent, "").is_none());
        assert!(sanitize_search_path(&parent, "..").is_none());
        let nested = format!("nested{}found.txt", std::path::MAIN_SEPARATOR);
        assert!(sanitize_search_path(&parent, &nested).is_none());
    }
}
