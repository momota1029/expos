//! Shell namespace clipboard publication on one process-owned OLE STA.
//!
//! UI requests contain detached parsing identities only. COM interfaces never
//! leave the STA that creates and publishes the data object.

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender, SyncSender},
};

#[derive(Clone, Debug)]
pub(crate) struct NamespaceCopy {
    pub parent: crate::location::Location,
    pub identities: Vec<String>,
}

pub(crate) enum PublicationGate {
    Busy,
    Acquired(std::sync::MutexGuard<'static, ()>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteTicket {
    Managed(u64),
    Rejected,
}

#[cfg(test)]
pub(crate) enum GateAttempt<G> {
    Busy,
    Acquired(G),
}

#[cfg(test)]
enum PublishAttempt<T, E> {
    Stale,
    Busy(T),
    Complete(Result<(), E>),
}

#[cfg(test)]
fn try_publish<T, E, G>(
    ticket: WriteTicket,
    prepared: T,
    gate: GateAttempt<G>,
    is_latest: &impl Fn(u64) -> bool,
    publish: &mut impl FnMut(T) -> Result<(), E>,
) -> PublishAttempt<T, E> {
    match gate {
        GateAttempt::Busy => PublishAttempt::Busy(prepared),
        GateAttempt::Acquired(_guard) => {
            if let WriteTicket::Managed(id) = ticket {
                if !is_latest(id) {
                    return PublishAttempt::Stale;
                }
            }
            PublishAttempt::Complete(publish(prepared))
        }
    }
}

#[cfg(test)]
/// Prepares a detached request, retries a busy publication gate without holding
/// its guard across an await, and revalidates managed tickets at publication.
pub(crate) async fn publish_with_ticket<
    T,
    E,
    G,
    Prepare,
    TryGate,
    IsLatest,
    Publish,
    Wait,
    WaitFuture,
>(
    ticket: WriteTicket,
    prepare: Prepare,
    mut try_gate: TryGate,
    is_latest: IsLatest,
    mut publish: Publish,
    mut wait: Wait,
) -> Option<Result<(), E>>
where
    Prepare: FnOnce() -> Result<T, E>,
    TryGate: FnMut() -> GateAttempt<G>,
    IsLatest: Fn(u64) -> bool,
    Publish: FnMut(T) -> Result<(), E>,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: std::future::Future<Output = ()>,
{
    if matches!(ticket, WriteTicket::Rejected) {
        return None;
    }
    let mut prepared = match prepare() {
        Ok(prepared) => prepared,
        Err(error) => return Some(Err(error)),
    };
    loop {
        if let WriteTicket::Managed(id) = ticket {
            if !is_latest(id) {
                return None;
            }
        }
        match try_publish(ticket, prepared, try_gate(), &is_latest, &mut publish) {
            PublishAttempt::Stale => return None,
            PublishAttempt::Complete(result) => return Some(result),
            PublishAttempt::Busy(request) => {
                prepared = request;
                wait().await;
            }
        }
    }
}

pub(crate) fn namespace_copy_allowed(
    parent: &crate::location::Location,
    identities: &[String],
) -> bool {
    use crate::location::{Location, ShellRoot};
    if !matches!(
        parent,
        Location::ShellRoot(root) | Location::ShellItem { root, .. }
            if *root != ShellRoot::RecycleBin
    ) || identities.is_empty()
    {
        return false;
    }
    let unique = identities.iter().collect::<std::collections::HashSet<_>>();
    unique.len() == identities.len() && identities.iter().all(|identity| !identity.is_empty())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyCandidate {
    Virtual(String),
    PathBacked,
    Other,
}

fn is_latest_ticket(ticket: u64, latest: u64) -> bool {
    ticket == latest
}

fn dispatch_file_write(
    ticket: u64,
    latest: u64,
    publish: impl FnOnce() -> Result<(), String>,
) -> Option<Result<(), String>> {
    is_latest_ticket(ticket, latest).then(publish)
}

fn flush_receipt(
    receipt: Option<(u64, Result<(), String>)>,
    expected_ticket: u64,
) -> Option<Result<(), String>> {
    receipt
        .filter(|(ticket, _)| *ticket == expected_ticket)
        .map(|(_, result)| result)
}

pub(crate) fn namespace_copy_selection(
    parent: &crate::location::Location,
    candidates: &[CopyCandidate],
    is_copy: bool,
) -> Option<Vec<String>> {
    if !is_copy {
        return None;
    }
    let identities = candidates
        .iter()
        .map(|candidate| match candidate {
            CopyCandidate::Virtual(identity) => Some(identity.clone()),
            CopyCandidate::PathBacked | CopyCandidate::Other => None,
        })
        .collect::<Option<Vec<_>>>()?;
    namespace_copy_allowed(parent, &identities).then_some(identities)
}

enum Command {
    Write { ticket: u64, request: NamespaceCopy },
    FileWrite {
        ticket: u64,
        prepared: crate::operations::file_clipboard::PreparedFileClipboard,
        completion: SyncSender<Option<Result<(), String>>>,
    },
    Flush { ticket: u64 },
}

struct Owner {
    sender: Sender<Command>,
    result: Arc<Mutex<Option<(u64, Result<(), String>)>>>,
    flush_ticket: Arc<AtomicU64>,
    flush_started: AtomicBool,
}

struct TicketState {
    latest: Arc<AtomicU64>,
    next_ticket: AtomicU64,
    gate: Arc<Mutex<()>>,
    accepting: Mutex<bool>,
}

impl TicketState {
    fn new() -> Self {
        Self {
            latest: Arc::new(AtomicU64::new(0)),
            next_ticket: AtomicU64::new(0),
            gate: Arc::new(Mutex::new(())),
            accepting: Mutex::new(true),
        }
    }

    fn issue(&self) -> Result<u64, String> {
        let accepting = self
            .accepting
            .lock()
            .map_err(|_| "clipboard admission lock poisoned".to_owned())?;
        if !*accepting {
            return Err("clipboard owner is closing".into());
        }
        self.issue_unchecked()
    }

    fn issue_unchecked(&self) -> Result<u64, String> {
        let ticket = self
            .next_ticket
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(1)
            })
            .map_err(|_| "clipboard ticket exhausted".to_owned())?
            + 1;
        // Concurrent issuers may store out of order; latest must never regress.
        self.latest.fetch_max(ticket, Ordering::AcqRel);
        Ok(ticket)
    }

    fn begin_close(&self) {
        if let Ok(mut accepting) = self.accepting.lock() {
            if *accepting {
                *accepting = false;
                let _ = self.issue_unchecked();
            }
        } else {
            self.latest.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn resume_accepting(&self) {
        if let Ok(mut accepting) = self.accepting.lock() {
            *accepting = true;
        }
    }

    fn issue_flush_ticket(&self) -> Result<u64, String> {
        let _accepting = self
            .accepting
            .lock()
            .map_err(|_| "clipboard admission lock poisoned".to_owned())?;
        self.issue_unchecked()
    }

    fn enqueue_if_current(
        &self,
        ticket: u64,
        enqueue: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let accepting = self
            .accepting
            .lock()
            .map_err(|_| "clipboard admission lock poisoned".to_owned())?;
        if !*accepting {
            return Err("clipboard owner is closing".into());
        }
        if self.latest.load(Ordering::Acquire) != ticket {
            return Err("clipboard write was superseded".into());
        }
        enqueue()
    }
}

static OWNER: OnceLock<Result<Owner, String>> = OnceLock::new();
static TICKETS: OnceLock<Arc<TicketState>> = OnceLock::new();
#[cfg(test)]
static FILE_WRITE_COMMANDS_FOR_TEST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn file_write_command_count_for_test() -> usize {
    FILE_WRITE_COMMANDS_FOR_TEST.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn owner_is_uninitialized_for_test() -> bool {
    OWNER.get().is_none()
}

fn tickets() -> &'static Arc<TicketState> {
    TICKETS.get_or_init(|| Arc::new(TicketState::new()))
}

fn owner() -> Result<&'static Owner, String> {
    OWNER
        .get_or_init(start_owner)
        .as_ref()
        .map_err(Clone::clone)
}

pub(crate) fn publish(ticket: u64, request: NamespaceCopy) -> Result<(), String> {
    let owner = owner()?;
    tickets().enqueue_if_current(ticket, || {
        owner
            .sender
            .send(Command::Write { ticket, request })
            .map_err(|_| "clipboard STA stopped".to_owned())
    })
}

/// Enqueues a prepared file clipboard write on the owner STA and waits for its
/// own publication result. The STA rechecks the ticket under the shared gate.
pub(crate) fn publish_file(
    ticket: WriteTicket,
    prepared: crate::operations::file_clipboard::PreparedFileClipboard,
) -> Option<Result<(), String>> {
    let WriteTicket::Managed(ticket) = ticket else {
        return None;
    };
    let owner = match owner() {
        Ok(owner) => owner,
        Err(error) => return Some(Err(error)),
    };
    let (completion_tx, completion_rx) = mpsc::sync_channel(1);
    let queued = tickets().enqueue_if_current(ticket, || {
        #[cfg(test)]
        FILE_WRITE_COMMANDS_FOR_TEST.fetch_add(1, Ordering::AcqRel);
        owner.sender.send(Command::FileWrite {
            ticket,
            prepared,
            completion: completion_tx,
        })
        .map_err(|_| "clipboard STA stopped".to_owned())
    });
    if let Err(error) = queued {
        return Some(Err(error));
    }
    completion_rx
        .recv()
        .unwrap_or_else(|_| Some(Err("clipboard STA stopped".into())))
}

pub(crate) fn request_write_ticket() -> WriteTicket {
    tickets()
        .issue()
        .map(WriteTicket::Managed)
        .unwrap_or(WriteTicket::Rejected)
}

pub(crate) fn is_latest(ticket: u64) -> bool {
    tickets().latest.load(Ordering::Acquire) == ticket
}

/// Runs a publication or receipt only while its captured managed ticket is current.
pub(crate) fn run_if_latest<T>(
    ticket: u64,
    latest: impl FnOnce(u64) -> bool,
    action: impl FnOnce() -> T,
) -> Option<T> {
    latest(ticket).then(action)
}

pub(crate) fn try_publication_gate() -> PublicationGate {
    match tickets().gate.try_lock() {
        Ok(guard) => PublicationGate::Acquired(guard),
        Err(std::sync::TryLockError::WouldBlock) => PublicationGate::Busy,
        Err(std::sync::TryLockError::Poisoned(_)) => PublicationGate::Busy,
    }
}

pub(crate) fn begin_final_close() {
    tickets().begin_close();
}

pub(crate) fn resume_accepting() {
    tickets().resume_accepting();
    if let Some(Ok(owner)) = OWNER.get() {
        owner.flush_started.store(false, Ordering::Release);
        let invalidated_ticket = owner.flush_ticket.swap(0, Ordering::AcqRel);
        if let Ok(mut result) = owner.result.lock() {
            if result
                .as_ref()
                .is_some_and(|(ticket, _)| *ticket == invalidated_ticket)
            {
                *result = None;
            }
        }
    }
}

/// Starts the final-window shutdown handshake. `Ok(None)` means the STA is
/// still flushing; `Ok(Some(()))` permits window removal. Failures are retained
/// and can be retried without accepting another clipboard write.
pub(crate) fn poll_final_close() -> Result<Option<()>, String> {
    begin_final_close();
    let Some(Ok(owner)) = OWNER.get() else {
        return Ok(Some(()));
    };
    let completed = owner
        .result
        .lock()
        .map_err(|_| "clipboard result lock poisoned".to_owned())?
        .take();
    let requested = owner.flush_ticket.load(Ordering::Acquire);
    if let Some(result) = flush_receipt(completed, requested) {
        match result {
            Ok(()) => return Ok(Some(())),
            Err(error) => {
                owner.flush_started.store(false, Ordering::Release);
                return Err(error);
            }
        }
    }
    if owner.flush_started.load(Ordering::Acquire) {
        return Ok(None);
    }
    let ticket = tickets().issue_flush_ticket()?;
    owner.flush_ticket.store(ticket, Ordering::Release);
    owner.flush_started.store(true, Ordering::Release);
    owner
        .sender
        .send(Command::Flush { ticket })
        .map_err(|_| "clipboard STA stopped".to_owned())?;
    Ok(None)
}

fn start_owner() -> Result<Owner, String> {
    let (sender, receiver) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let ticket_state = tickets().clone();
    let latest = ticket_state.latest.clone();
    let result = Arc::new(Mutex::new(None));
    let thread_latest = latest.clone();
    let thread_result = result.clone();
    let gate = ticket_state.gate.clone();
    let sta_gate = gate.clone();
    let thread_flush_ticket = Arc::new(AtomicU64::new(0));
    let sta_flush_ticket = thread_flush_ticket.clone();
    std::thread::Builder::new()
        .name("gpui-filer-clipboard-ole-sta".into())
        .spawn(move || {
            sta_main(
                receiver,
                ready_tx,
                thread_latest,
                thread_result,
                sta_flush_ticket,
                sta_gate,
            )
        })
        .map_err(|error| format!("start clipboard STA: {error}"))?;
    ready_rx
        .recv()
        .map_err(|_| "clipboard STA exited during initialization".to_owned())??;
    Ok(Owner {
        sender,
        result,
        flush_ticket: thread_flush_ticket,
        flush_started: AtomicBool::new(false),
    })
}

#[cfg(windows)]
fn sta_main(
    receiver: Receiver<Command>,
    ready: SyncSender<Result<(), String>>,
    latest: Arc<AtomicU64>,
    result: Arc<Mutex<Option<(u64, Result<(), String>)>>>,
    flush_ticket: Arc<AtomicU64>,
    gate: Arc<Mutex<()>>,
) {
    use windows_062::Win32::{
        System::Ole::OleInitialize,
        UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE, WS_OVERLAPPED},
    };
    let initialized = unsafe { OleInitialize(None) };
    if initialized.is_err() {
        let _ = ready.send(Err(format!("initialize clipboard STA: {initialized:?}")));
        return;
    }
    let _ = initialized.ok();
    let _ole = OleUninitialize;
    #[cfg(test)]
    if let Ok(mode) = std::env::var("GPUI_FILER_TEST_CLIPBOARD_STARTUP_FAILURE") {
        if mode == "current" || mode == "stale" {
            // 子プロセス内だけで owner-window 作成失敗を再現し、実際の readiness error 経路を通す。
            let _ = ready.send(Err("injected clipboard owner window creation failure".into()));
            return;
        }
    }
    // A private message-only window gives clipboard publication a valid owner
    // without tying the process STA to any GPUI window lifetime.
    let owner_hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            windows_062::core::w!("STATIC"),
            windows_062::core::w!("gpui-filer-clipboard-owner"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let owner_hwnd = match owner_hwnd {
        Ok(hwnd) if !hwnd.is_invalid() => hwnd,
        result => {
            let _ = ready.send(Err(format!("create clipboard owner window: {result:?}")));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    loop {
        pump_messages();
        match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(Command::FileWrite {
                ticket,
                prepared,
                completion,
            }) => {
                let outcome = match gate.lock() {
                    Ok(_gate) => {
                        dispatch_file_write(ticket, latest.load(Ordering::Acquire), || {
                            let owner = std::ptr::NonNull::new(owner_hwnd.0)
                                .expect("created clipboard owner HWND must be non-null");
                            crate::operations::file_clipboard::publish_prepared(prepared, owner)
                                .map_err(|error| error.to_string())
                        })
                    }
                    Err(_) => Some(Err("clipboard publication gate poisoned".into())),
                };
                let _ = completion.send(outcome);
            }
            Ok(Command::Write { ticket, request }) => {
                let outcome = if is_latest_ticket(ticket, latest.load(Ordering::Acquire)) {
                    unsafe { prepare_on_sta(&request) }.and_then(|data| match gate.lock() {
                            Ok(_gate)
                                if is_latest_ticket(ticket, latest.load(Ordering::Acquire)) =>
                            unsafe { windows_062::Win32::System::Ole::OleSetClipboard(&data) },
                            Ok(_) => Err(superseded_error()),
                            Err(_) => Err(windows_062::core::Error::from_hresult(
                                windows_062::core::HRESULT(0x8004_0002u32 as i32),
                            )),
                        })
                } else {
                    Err(superseded_error())
                };
                if let Ok(mut slot) = result.lock() {
                    *slot = Some((ticket, outcome.map_err(|error| error.to_string())));
                }
            }
            Ok(Command::Flush { ticket }) => {
                let flush = if flush_ticket.load(Ordering::Acquire) == ticket {
                    unsafe { ole_flush() }.map_err(|error| error.to_string())
                } else {
                    continue;
                };
                // Keep the process owner alive after success: a new window may appear while
                // the closing window is waiting on this receipt.
                if let Ok(mut slot) = result.lock() {
                    *slot = Some((ticket, flush));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    unsafe {
        let _ = DestroyWindow(owner_hwnd);
    }
    drop(_ole);
}

#[cfg(not(windows))]
fn sta_main(
    _receiver: Receiver<Command>,
    ready: SyncSender<Result<(), String>>,
    _latest: Arc<AtomicU64>,
    _result: Arc<Mutex<Option<(u64, Result<(), String>)>>>,
    _flush_ticket: Arc<AtomicU64>,
    _gate: Arc<Mutex<()>>,
) {
    let _ = ready.send(Err(
        "Shell namespace clipboard is only supported on Windows".into(),
    ));
}

#[cfg(windows)]
struct OleUninitialize;

#[cfg(windows)]
impl Drop for OleUninitialize {
    fn drop(&mut self) {
        unsafe { windows_062::Win32::System::Ole::OleUninitialize() };
    }
}

#[cfg(windows)]
unsafe fn prepare_on_sta(
    request: &NamespaceCopy,
) -> windows_062::core::Result<windows_062::Win32::System::Com::IDataObject> {
    use std::ptr;
    use windows_062::Win32::System::Com::IDataObject;
    use windows_062::{
        Win32::{
            System::Com::IBindCtx,
            UI::Shell::{
                Common::ITEMIDLIST, FOLDERID_Libraries, FOLDERID_NetworkFolder, ILClone,
                ILFindLastID, ILIsEqual, ILRemoveLastID, KF_FLAG_DEFAULT, SHCreateDataObject,
                SHGetKnownFolderIDList, SHParseDisplayName,
            },
        },
        core::PCWSTR,
    };

    if !namespace_copy_allowed(&request.parent, &request.identities) {
        return Err(windows_062::core::Error::from_thread());
    }
    let expected_parent = match &request.parent {
        crate::location::Location::ShellRoot(crate::location::ShellRoot::Libraries) => unsafe {
            SHGetKnownFolderIDList(&FOLDERID_Libraries, KF_FLAG_DEFAULT.0 as u32, None)?
        },
        crate::location::Location::ShellRoot(crate::location::ShellRoot::Network) => unsafe {
            SHGetKnownFolderIDList(&FOLDERID_NetworkFolder, KF_FLAG_DEFAULT.0 as u32, None)?
        },
        crate::location::Location::ShellItem { parsing_name, .. } => {
            parse_shell_identity(parsing_name)?
        }
        _ => {
            return Err(windows_062::core::Error::from_hresult(
                windows_062::core::HRESULT(0x8007_0057u32 as i32),
            ));
        }
    };
    let expected_parent = Pidl(expected_parent);
    let mut absolute = Vec::<*mut ITEMIDLIST>::with_capacity(request.identities.len());
    for identity in &request.identities {
        let wide = identity.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut pidl = ptr::null_mut();
        unsafe {
            SHParseDisplayName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>, &mut pidl, 0, None)?;
        }
        absolute.push(pidl);
    }
    let _pidls = PidlList(absolute);
    let first_parent = Pidl(unsafe { ILClone(_pidls.0[0]) });
    if first_parent.0.is_null() {
        return Err(windows_062::core::Error::from_thread());
    }
    if !unsafe { ILRemoveLastID(Some(first_parent.0)) }.as_bool() {
        return Err(windows_062::core::Error::from_hresult(
            windows_062::core::HRESULT(0x8007_0057u32 as i32),
        ));
    }
    if !unsafe { ILIsEqual(expected_parent.0, first_parent.0) }.as_bool() {
        return Err(windows_062::core::Error::from_hresult(
            windows_062::core::HRESULT(0x8007_0057u32 as i32),
        ));
    }
    for pidl in _pidls.0.iter().skip(1) {
        let candidate = unsafe { ILClone(*pidl) };
        if candidate.is_null() {
            return Err(windows_062::core::Error::from_thread());
        }
        if !unsafe { ILRemoveLastID(Some(candidate)) }.as_bool() {
            unsafe { windows_062::Win32::UI::Shell::ILFree(Some(candidate)) };
            return Err(windows_062::core::Error::from_hresult(
                windows_062::core::HRESULT(0x8007_0057u32 as i32),
            ));
        }
        let same_parent = unsafe { ILIsEqual(first_parent.0, candidate) }.as_bool();
        unsafe { windows_062::Win32::UI::Shell::ILFree(Some(candidate)) };
        if !same_parent {
            return Err(windows_062::core::Error::from_thread());
        }
    }
    let children = _pidls
        .0
        .iter()
        .map(|pidl| unsafe { ILFindLastID(*pidl) as *const ITEMIDLIST })
        .collect::<Vec<_>>();
    if children.iter().any(|child| child.is_null()) {
        return Err(windows_062::core::Error::from_hresult(
            windows_062::core::HRESULT(0x8007_0057u32 as i32),
        ));
    }
    let data: IDataObject = unsafe {
        SHCreateDataObject::<_, IDataObject>(
            Some(first_parent.0),
            Some(&children),
            None::<&IDataObject>,
        )?
    };
    Ok(data)
}

#[cfg(windows)]
fn superseded_error() -> windows_062::core::Error {
    windows_062::core::Error::from_hresult(windows_062::core::HRESULT(0x8004_0001u32 as i32))
}

#[cfg(windows)]
fn parse_shell_identity(
    identity: &str,
) -> windows_062::core::Result<*mut windows_062::Win32::UI::Shell::Common::ITEMIDLIST> {
    let wide = identity.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut pidl = std::ptr::null_mut();
    unsafe {
        windows_062::Win32::UI::Shell::SHParseDisplayName(
            windows_062::core::PCWSTR(wide.as_ptr()),
            None::<&windows_062::Win32::System::Com::IBindCtx>,
            &mut pidl,
            0,
            None,
        )?;
    }
    Ok(pidl)
}

#[cfg(windows)]
struct PidlList(Vec<*mut windows_062::Win32::UI::Shell::Common::ITEMIDLIST>);

#[cfg(windows)]
impl Drop for PidlList {
    fn drop(&mut self) {
        for pidl in &self.0 {
            unsafe { windows_062::Win32::UI::Shell::ILFree(Some(*pidl)) };
        }
    }
}

#[cfg(windows)]
struct Pidl(*mut windows_062::Win32::UI::Shell::Common::ITEMIDLIST);

#[cfg(windows)]
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { windows_062::Win32::UI::Shell::ILFree(Some(self.0)) };
    }
}

#[cfg(windows)]
unsafe fn ole_flush() -> windows_062::core::Result<()> {
    unsafe { windows_062::Win32::System::Ole::OleFlushClipboard() }
}

#[cfg(windows)]
fn pump_messages() {
    use windows_062::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    loop {
        let mut message = MSG::default();
        if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_request_preserves_ordered_identities() {
        let request = NamespaceCopy {
            parent: crate::location::Location::ShellRoot(crate::location::ShellRoot::Libraries),
            identities: vec!["first".into(), "second".into()],
        };
        assert_eq!(request.identities, ["first", "second"]);
    }

    #[test]
    fn namespace_copy_requires_non_recycle_homogeneous_identities() {
        use crate::location::{Location, ShellRoot};
        let parent = Location::ShellRoot(ShellRoot::Libraries);
        assert!(namespace_copy_allowed(
            &parent,
            &["one".into(), "two".into()]
        ));
        assert!(!namespace_copy_allowed(&parent, &[]));
        assert!(!namespace_copy_allowed(
            &parent,
            &["same".into(), "same".into()]
        ));
        assert!(!namespace_copy_allowed(
            &Location::ShellRoot(ShellRoot::RecycleBin),
            &["recycle-item".into()]
        ));
        assert!(!namespace_copy_allowed(
            &Location::Path("C:/".into()),
            &["one".into()]
        ));
        assert_eq!(
            namespace_copy_selection(&parent, &[CopyCandidate::Virtual("one".into())], true,),
            Some(vec!["one".into()])
        );
        assert!(
            namespace_copy_selection(
                &parent,
                &[
                    CopyCandidate::Virtual("one".into()),
                    CopyCandidate::PathBacked
                ],
                true,
            )
            .is_none()
        );
        assert!(namespace_copy_selection(&parent, &[CopyCandidate::PathBacked], true,).is_none());
        assert!(
            namespace_copy_selection(&parent, &[CopyCandidate::Virtual("one".into())], false,)
                .is_none()
        );
    }

    #[test]
    fn latest_ticket_and_flush_receipt_reject_stale_results() {
        assert!(!is_latest_ticket(4, 5));
        assert!(is_latest_ticket(5, 5));
        assert!(flush_receipt(Some((8, Ok(()))), 9).is_none());
        assert_eq!(flush_receipt(Some((9, Ok(()))), 9), Some(Ok(())));
        assert_eq!(
            flush_receipt(Some((9, Err("flush failed".into()))), 9),
            Some(Err("flush failed".into()))
        );
    }

    #[test]
    fn file_write_dispatch_rejects_stale_ticket_without_publishing() {
        let publications = std::cell::Cell::new(0);
        let result = dispatch_file_write(4, 5, || {
            publications.set(publications.get() + 1);
            Ok(())
        });
        assert_eq!(result, None);
        assert_eq!(publications.get(), 0);
    }

    #[test]
    fn file_write_dispatch_returns_its_publication_result() {
        let result = dispatch_file_write(5, 5, || Err("SetClipboardData failed".into()));
        assert_eq!(result, Some(Err("SetClipboardData failed".into())));
    }

    #[test]
    fn ordinary_write_a_is_superseded_by_ordinary_write_b_without_owner() {
        assert_held_write_is_superseded_by_new_no_owner_request();
    }

    #[test]
    fn ordinary_write_a_is_superseded_by_namespace_write_b_without_owner() {
        assert_held_write_is_superseded_by_new_no_owner_request();
    }

    fn assert_held_write_is_superseded_by_new_no_owner_request() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        };

        let state = Arc::new(TicketState::new());
        let WriteTicket::Managed(ticket_a) = state.issue().map(WriteTicket::Managed).unwrap()
        else {
            panic!("owner absence must not make ordinary writes unmanaged");
        };
        let publication_count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let worker_state = state.clone();
        let worker_publications = publication_count.clone();
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            let future = publish_with_ticket(
                WriteTicket::Managed(ticket_a),
                || {
                    // A is held in path preparation before the gate/publication check.
                    worker_barrier.wait();
                    worker_barrier.wait();
                    Ok::<_, ()>(vec![1_u8])
                },
                || GateAttempt::Acquired(()),
                |ticket| is_latest_ticket(ticket, worker_state.latest.load(Ordering::Acquire)),
                |payload| {
                    worker_publications.fetch_add(payload.len(), Ordering::AcqRel);
                    Ok(())
                },
                || std::future::ready(()),
            );
            block_on_ready(future)
        });

        barrier.wait();
        // Both ordinary and namespace requests use this shared ticket API without starting OLE.
        let WriteTicket::Managed(ticket_b) = state.issue().map(WriteTicket::Managed).unwrap()
        else {
            panic!("new ownerless clipboard request should be accepted");
        };
        assert!(ticket_b > ticket_a);
        // The foreground route remains able to process another action while A waits.
        let foreground_actions = AtomicUsize::new(0);
        foreground_actions.fetch_add(1, Ordering::Relaxed);
        barrier.wait();

        assert_eq!(worker.join().unwrap(), None);
        assert_eq!(publication_count.load(Ordering::Acquire), 0);
        let stale_receipt_applied = run_if_latest(
            ticket_a,
            |ticket| is_latest_ticket(ticket, state.latest.load(Ordering::Acquire)),
            || foreground_actions.fetch_add(1, Ordering::Relaxed),
        );
        assert_eq!(stale_receipt_applied, None);
        assert_eq!(foreground_actions.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn managed_publication_waits_for_a_busy_gate_then_publishes_once() {
        use std::cell::Cell;

        let gate_attempts = Cell::new(0);
        let waits = Cell::new(0);
        let publications = Cell::new(0);
        let result = block_on_ready(publish_with_ticket(
            WriteTicket::Managed(7),
            || Ok::<_, ()>("prepared"),
            || {
                let attempt = gate_attempts.get();
                gate_attempts.set(attempt + 1);
                if attempt == 0 {
                    GateAttempt::Busy
                } else {
                    GateAttempt::Acquired(())
                }
            },
            |ticket| ticket == 7,
            |payload| {
                assert_eq!(payload, "prepared");
                publications.set(publications.get() + 1);
                Ok(())
            },
            || {
                waits.set(waits.get() + 1);
                std::future::ready(())
            },
        ));

        assert_eq!(result, Some(Ok(())));
        assert_eq!(gate_attempts.get(), 2);
        assert_eq!(waits.get(), 1);
        assert_eq!(publications.get(), 1);
    }

    #[test]
    fn final_close_during_delayed_owner_startup_rejects_namespace_enqueue() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        };

        let state = Arc::new(TicketState::new());
        let WriteTicket::Managed(ticket) = state.issue().map(WriteTicket::Managed).unwrap() else {
            unreachable!();
        };
        let barrier = Arc::new(Barrier::new(2));
        let enqueued = Arc::new(AtomicUsize::new(0));
        let thread_state = state.clone();
        let thread_barrier = barrier.clone();
        let thread_enqueued = enqueued.clone();
        let delayed_startup = std::thread::spawn(move || {
            // Represents owner/STA startup completing after the final close poll.
            thread_barrier.wait();
            thread_barrier.wait();
            thread_state.enqueue_if_current(ticket, || {
                thread_enqueued.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        });

        barrier.wait();
        state.begin_close();
        barrier.wait();

        assert!(delayed_startup.join().unwrap().is_err());
        assert_eq!(enqueued.load(Ordering::Acquire), 0);
        assert!(!is_latest_ticket(
            ticket,
            state.latest.load(Ordering::Acquire)
        ));
        assert!(state.issue().is_err());
    }

    fn block_on_ready<F: std::future::Future>(future: F) -> F::Output {
        use std::{
            sync::Arc,
            task::{Context, Poll, Wake, Waker},
        };
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test publication unexpectedly waited"),
        }
    }
}
