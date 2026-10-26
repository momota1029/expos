//! 7-Zip payload callback adapters and invocation-local bounded spool state.
//!
//! SDK の COM 呼び出しと callback state の寿命をここで閉じ、archive/provider
//! の外へ生ポインターや spool の path authority を出さない。

use std::{
    ffi::c_void,
    fs::File,
    io::{Seek, SeekFrom, Write},
    ops::Range,
    ptr, slice,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use super::super::seven_zip_extract_plan::Validated7zExtractPlan;
use super::{
    E_FAIL, E_INVALIDARG, IID_I_ARCHIVE_EXTRACT_CALLBACK, IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2,
    IID_I_PROGRESS, IID_I_SEQUENTIAL_OUT_STREAM, IID_IUNKNOWN, S_OK, Unknown, UnknownVtable,
};
use std::path::Path;

use crate::archive_manifest::NodeKind;

#[cfg(test)]
#[path = "seven_zip_extract_sdk_tests.rs"]
mod sdk_fixture_tests;

const EXTRACT_MODE: u32 = 0;
const E_ABORT: i32 = 0x8000_4004u32 as i32;

type GetStreamFn =
    unsafe extern "system" fn(*mut ExtractCallback, u32, *mut *mut Unknown, u32) -> i32;
type PrepareOperationFn = unsafe extern "system" fn(*mut ExtractCallback, u32) -> i32;
type SetOperationResultFn = unsafe extern "system" fn(*mut ExtractCallback, i32) -> i32;
type ReportExtractResultFn = unsafe extern "system" fn(*mut ExtractMessage, u32, u32, i32) -> i32;
type SetTotalFn = unsafe extern "system" fn(*mut ExtractCallback, u64) -> i32;
type SetCompletedFn = unsafe extern "system" fn(*mut ExtractCallback, *const u64) -> i32;
type WriteFn = unsafe extern "system" fn(*mut OutputStream, *const c_void, u32, *mut u32) -> i32;

#[repr(C)]
struct ProgressVtable {
    unknown: UnknownVtable,
    set_total: SetTotalFn,
    set_completed: SetCompletedFn,
}

#[repr(C)]
struct ExtractCallbackVtable {
    progress: ProgressVtable,
    get_stream: GetStreamFn,
    prepare_operation: PrepareOperationFn,
    set_operation_result: SetOperationResultFn,
}

#[repr(C)]
struct ExtractMessageVtable {
    unknown: UnknownVtable,
    report_extract_result: ReportExtractResultFn,
}

#[repr(C)]
struct OutputVtable {
    unknown: UnknownVtable,
    write: WriteFn,
}

#[repr(C)]
struct ExtractCallback {
    vtable: *const ExtractCallbackVtable,
    refs: AtomicU32,
    invocation: Arc<DecodeInvocation>,
    message: ExtractMessage,
}

#[repr(C)]
struct ExtractMessage {
    vtable: *const ExtractMessageVtable,
    owner: *mut ExtractCallback,
}

#[repr(C)]
struct OutputStream {
    vtable: *const OutputVtable,
    refs: AtomicU32,
    invocation: Arc<DecodeInvocation>,
    token: u64,
    index: u32,
}

struct PendingExtract {
    invocation: Arc<DecodeInvocation>,
    // Close と archive の最終 Release が終わるまで COM の owner ref を保持する。
    callback: CallbackOwner,
    extract_succeeded: bool,
    sealed: Result<(), DecodeFailure>,
}

struct CallbackOwner(*mut ExtractCallback);

impl Drop for CallbackOwner {
    fn drop(&mut self) {
        unsafe {
            extract_release(self.0.cast());
        }
    }
}

fn new_callback_owner(invocation: Arc<DecodeInvocation>) -> CallbackOwner {
    let callback = Box::new(ExtractCallback {
        vtable: &EXTRACT_CALLBACK_VTABLE,
        refs: AtomicU32::new(1),
        invocation,
        message: ExtractMessage {
            vtable: &EXTRACT_MESSAGE_VTABLE,
            owner: ptr::null_mut(),
        },
    });
    let callback = Box::into_raw(callback);
    unsafe {
        // COM で安定した owner address を得た後に embedded interface を接続する。
        (*callback).message.owner = callback;
    }
    CallbackOwner(callback)
}

pub(super) fn extract_archive(
    dll: &Path,
    source: File,
    expected_length: u64,
    expected_plan: Arc<Validated7zExtractPlan>,
    spool: File,
    cancelled: Arc<AtomicBool>,
) -> Result<(), i32> {
    extract_archive_inner(
        dll,
        source,
        expected_length,
        expected_plan,
        spool,
        cancelled,
        None,
    )
}

#[cfg(test)]
fn extract_archive_observed(
    dll: &Path,
    source: File,
    expected_length: u64,
    expected_plan: Arc<Validated7zExtractPlan>,
    spool: File,
    cancelled: Arc<AtomicBool>,
) -> (Result<(), i32>, Arc<AtomicBool>) {
    let reported_after_all_done = Arc::new(AtomicBool::new(false));
    let result = extract_archive_inner(
        dll,
        source,
        expected_length,
        expected_plan,
        spool,
        cancelled,
        Some(reported_after_all_done.clone()),
    );
    (result, reported_after_all_done)
}

fn extract_archive_inner(
    dll: &Path,
    source: File,
    expected_length: u64,
    expected_plan: Arc<Validated7zExtractPlan>,
    spool: File,
    cancelled: Arc<AtomicBool>,
    report_observer: Option<Arc<AtomicBool>>,
) -> Result<(), i32> {
    super::read_archive_records_with(
        dll,
        source,
        expected_length,
        true,
        |archive, records| {
            let actual_plan =
                Validated7zExtractPlan::validate(records.to_vec()).map_err(|_| E_FAIL)?;
            if &actual_plan != expected_plan.as_ref() {
                return Err(E_FAIL);
            }
            let invocation = Arc::new(
                DecodeInvocation::new_observed(
                    expected_plan.clone(),
                    spool,
                    cancelled,
                    report_observer,
                )
                .map_err(failure_hresult)?,
            );
            invocation.check_progress().map_err(failure_hresult)?;
            let callback = new_callback_owner(invocation.clone());
            let indices = expected_plan
                .entries()
                .iter()
                .map(|entry| entry.index())
                .collect::<Vec<_>>();
            let result = unsafe {
                ((*(*archive).vtable).extract)(
                    archive,
                    indices.as_ptr(),
                    indices.len() as u32,
                    0,
                    callback.0.cast(),
                )
            };
            // Extract が戻った時点で seal し、Close 中の遅延 callback に書かせない。
            let sealed = invocation.seal();
            Ok(PendingExtract {
                invocation,
                callback,
                extract_succeeded: result == S_OK.0,
                sealed,
            })
        },
        |pending, source_failed| {
            let PendingExtract {
                invocation,
                callback,
                extract_succeeded,
                sealed,
            } = pending;
            let _keep_callback_owner_alive = callback;
            if source_failed {
                invocation.reject_callback(DecodeFailure::Io);
            }
            sealed.map_err(failure_hresult)?;
            invocation
                .finish(extract_succeeded && !source_failed)
                .map_err(failure_hresult)
        },
    )?;
    Ok(())
}

fn failure_hresult(failure: DecodeFailure) -> i32 {
    match failure {
        DecodeFailure::Cancelled => E_ABORT,
        _ => E_FAIL,
    }
}

static EXTRACT_CALLBACK_VTABLE: ExtractCallbackVtable = ExtractCallbackVtable {
    progress: ProgressVtable {
        unknown: UnknownVtable {
            query_interface: extract_query_interface,
            add_ref: extract_add_ref,
            release: extract_release,
        },
        set_total: progress_set_total,
        set_completed: progress_set_completed,
    },
    get_stream: extract_get_stream,
    prepare_operation: extract_prepare,
    set_operation_result: extract_set_result,
};

static EXTRACT_MESSAGE_VTABLE: ExtractMessageVtable = ExtractMessageVtable {
    unknown: UnknownVtable {
        query_interface: message_query_interface,
        add_ref: message_add_ref,
        release: message_release,
    },
    report_extract_result,
};

static OUTPUT_VTABLE: OutputVtable = OutputVtable {
    unknown: UnknownVtable {
        query_interface: output_query_interface,
        add_ref: output_add_ref,
        release: output_release,
    },
    write: output_write,
};

unsafe extern "system" fn extract_query_interface(
    this: *mut Unknown,
    iid: *const super::GUID,
    out: *mut *mut c_void,
) -> i32 {
    if this.is_null() || iid.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    *out = ptr::null_mut();
    if *iid == IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2 {
        let callback = this.cast::<ExtractCallback>();
        extract_add_ref(this);
        *out = ptr::addr_of_mut!((*callback).message).cast();
        return S_OK.0;
    }
    if *iid != IID_IUNKNOWN && *iid != IID_I_PROGRESS && *iid != IID_I_ARCHIVE_EXTRACT_CALLBACK {
        return 0x8000_4002u32 as i32;
    }
    extract_add_ref(this);
    *out = this.cast();
    S_OK.0
}

unsafe extern "system" fn message_query_interface(
    this: *mut Unknown,
    iid: *const super::GUID,
    out: *mut *mut c_void,
) -> i32 {
    if this.is_null() || iid.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    *out = ptr::null_mut();
    let message = this.cast::<ExtractMessage>();
    let owner = (*message).owner;
    if owner.is_null() {
        return E_FAIL;
    }
    let primary = owner.cast::<Unknown>();
    if *iid == IID_IUNKNOWN || *iid == IID_I_PROGRESS || *iid == IID_I_ARCHIVE_EXTRACT_CALLBACK {
        extract_add_ref(primary);
        *out = primary.cast();
        return S_OK.0;
    }
    if *iid == IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2 {
        extract_add_ref(primary);
        *out = message.cast();
        return S_OK.0;
    }
    0x8000_4002u32 as i32
}

unsafe extern "system" fn message_add_ref(this: *mut Unknown) -> u32 {
    if this.is_null() {
        return 0;
    }
    let owner = (*this.cast::<ExtractMessage>()).owner;
    if owner.is_null() {
        0
    } else {
        extract_add_ref(owner.cast())
    }
}

unsafe extern "system" fn message_release(this: *mut Unknown) -> u32 {
    if this.is_null() {
        return 0;
    }
    let owner = (*this.cast::<ExtractMessage>()).owner;
    if owner.is_null() {
        0
    } else {
        extract_release(owner.cast())
    }
}

unsafe extern "system" fn report_extract_result(
    this: *mut ExtractMessage,
    _index_type: u32,
    _index: u32,
    operation_result: i32,
) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let owner = (*this).owner;
    if owner.is_null() {
        return E_FAIL;
    }
    match (*owner).invocation.report_extract_result(operation_result) {
        Ok(()) => S_OK.0,
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn extract_add_ref(this: *mut Unknown) -> u32 {
    if this.is_null() {
        return 0;
    }
    let object = this.cast::<ExtractCallback>();
    match (*object)
        .refs
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            count.checked_add(1)
        }) {
        Ok(previous) => previous + 1,
        Err(current) => current,
    }
}

unsafe extern "system" fn extract_release(this: *mut Unknown) -> u32 {
    if this.is_null() {
        return 0;
    }
    let object = this.cast::<ExtractCallback>();
    match (*object)
        .refs
        .fetch_update(Ordering::Release, Ordering::Relaxed, |count| {
            count.checked_sub(1)
        }) {
        Ok(1) => {
            std::sync::atomic::fence(Ordering::Acquire);
            drop(Box::from_raw(object));
            0
        }
        Ok(previous) => previous - 1,
        Err(current) => current,
    }
}

unsafe extern "system" fn output_query_interface(
    this: *mut Unknown,
    iid: *const super::GUID,
    out: *mut *mut c_void,
) -> i32 {
    if this.is_null() || iid.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    *out = ptr::null_mut();
    if *iid != IID_IUNKNOWN && *iid != IID_I_SEQUENTIAL_OUT_STREAM {
        return 0x8000_4002u32 as i32;
    }
    output_add_ref(this);
    *out = this.cast();
    S_OK.0
}

unsafe extern "system" fn output_add_ref(this: *mut Unknown) -> u32 {
    if this.is_null() {
        return 0;
    }
    let object = this.cast::<OutputStream>();
    match (*object)
        .refs
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            count.checked_add(1)
        }) {
        Ok(previous) => previous + 1,
        Err(current) => current,
    }
}

unsafe extern "system" fn output_release(this: *mut Unknown) -> u32 {
    if this.is_null() {
        return 0;
    }
    let object = this.cast::<OutputStream>();
    match (*object)
        .refs
        .fetch_update(Ordering::Release, Ordering::Relaxed, |count| {
            count.checked_sub(1)
        }) {
        Ok(1) => {
            std::sync::atomic::fence(Ordering::Acquire);
            drop(Box::from_raw(object));
            0
        }
        Ok(previous) => previous - 1,
        Err(current) => current,
    }
}

unsafe extern "system" fn progress_set_total(this: *mut ExtractCallback, _: u64) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    match (*this).invocation.check_progress() {
        Ok(()) => S_OK.0,
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn progress_set_completed(this: *mut ExtractCallback, _: *const u64) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    match (*this).invocation.check_progress() {
        Ok(()) => S_OK.0,
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn extract_get_stream(
    this: *mut ExtractCallback,
    index: u32,
    out: *mut *mut Unknown,
    mode: u32,
) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    if out.is_null() {
        (*this)
            .invocation
            .reject_callback(DecodeFailure::InvalidArgument);
        return E_INVALIDARG;
    }
    *out = ptr::null_mut();
    match (*this).invocation.offer(index, mode) {
        Ok(Some(offer)) => {
            if offer.range.is_none() {
                return S_OK.0;
            }
            let stream = Box::new(OutputStream {
                vtable: &OUTPUT_VTABLE,
                refs: AtomicU32::new(1),
                invocation: (*this).invocation.clone(),
                token: offer.token,
                index,
            });
            *out = Box::into_raw(stream).cast();
            S_OK.0
        }
        Ok(None) => S_OK.0,
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn extract_prepare(this: *mut ExtractCallback, mode: u32) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    match (*this).invocation.prepare(mode) {
        Ok(()) => S_OK.0,
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn extract_set_result(this: *mut ExtractCallback, result: i32) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    match (*this).invocation.result(result as u32) {
        Ok(()) => S_OK.0,
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn output_write(
    this: *mut OutputStream,
    data: *const c_void,
    size: u32,
    processed: *mut u32,
) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    if !processed.is_null() {
        *processed = 0;
    }
    if data.is_null() && size != 0 {
        (*this)
            .invocation
            .reject_callback(DecodeFailure::InvalidArgument);
        return E_INVALIDARG;
    }
    let bytes = if size == 0 {
        &[]
    } else {
        slice::from_raw_parts(data.cast::<u8>(), size as usize)
    };
    match (*this)
        .invocation
        .write((*this).token, (*this).index, bytes)
    {
        Ok(count) => {
            if !processed.is_null() {
                *processed = count as u32;
            }
            S_OK.0
        }
        Err(DecodeFailure::Cancelled) => E_ABORT,
        Err(_) => E_FAIL,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Running,
    Sealed,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryPhase {
    Unseen,
    Offered { token: u64, mode: u32 },
    Prepared { token: u64, mode: u32 },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryFact {
    phase: EntryPhase,
    written: u64,
}

#[derive(Debug)]
struct CurrentSlot {
    index: u32,
    token: u64,
    mode: u32,
    written: u64,
}

struct State {
    spool: File,
    phase: Phase,
    current: Option<CurrentSlot>,
    entries: Vec<EntryFact>,
    next_token: u64,
    failure: Option<DecodeFailure>,
}

/// payload decode の失敗。呼び出し元はどの値も terminal として扱う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeFailure {
    InvalidSpool,
    InvalidPlan,
    InvalidArgument,
    Cancelled,
    WrongPhase,
    WrongMode,
    UnknownIndex,
    DuplicateIndex,
    CallbackOrder,
    InvalidOperationResult,
    LengthMismatch,
    RangeExceeded,
    Io,
    Incomplete,
    Poisoned,
}

/// GetStream 相当の admission が返す entry 情報。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutputOffer {
    pub(crate) token: u64,
    pub(crate) index: u32,
    /// directory は stream なし、空 file は `Some(n..n)` の range を持つ。
    pub(crate) range: Option<Range<u64>>,
}

/// 同期 SDK extraction 一回分。書き込みと progress/cancel 判定の競合を防ぐため、
/// 変更可能な事実をひとつの mutex で守る。
pub(crate) struct DecodeInvocation {
    plan: Arc<Validated7zExtractPlan>,
    cancelled: Arc<AtomicBool>,
    report_observer: Option<Arc<AtomicBool>>,
    state: Mutex<State>,
}

impl DecodeInvocation {
    #[cfg(test)]
    pub(crate) fn new(
        plan: Arc<Validated7zExtractPlan>,
        spool: File,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, DecodeFailure> {
        Self::new_observed(plan, spool, cancelled, None)
    }

    fn new_observed(
        plan: Arc<Validated7zExtractPlan>,
        spool: File,
        cancelled: Arc<AtomicBool>,
        report_observer: Option<Arc<AtomicBool>>,
    ) -> Result<Self, DecodeFailure> {
        if plan.entries().iter().enumerate().any(|(i, entry)| {
            usize::try_from(entry.index()).ok() != Some(i)
                || entry.kind() == NodeKind::File
                    && (entry.declared_size().is_none() || entry.spool_range().is_none())
                || entry.kind() == NodeKind::Directory
                    && (entry.declared_size().is_some_and(|size| size != 0)
                        || entry.spool_range().is_some())
        }) {
            return Err(DecodeFailure::InvalidPlan);
        }
        let metadata = spool.metadata().map_err(|_| DecodeFailure::InvalidSpool)?;
        if !metadata.is_file() || metadata.len() != 0 {
            return Err(DecodeFailure::InvalidSpool);
        }
        let entries = vec![
            EntryFact {
                phase: EntryPhase::Unseen,
                written: 0,
            };
            plan.entries().len()
        ];
        Ok(Self {
            plan,
            cancelled,
            report_observer,
            state: Mutex::new(State {
                spool,
                phase: Phase::Running,
                current: None,
                entries,
                next_token: 1,
                failure: None,
            }),
        })
    }

    /// SDK が指定した entry を受け入れる。extract mode 以外や不正 callback は失敗を固定する。
    pub(crate) fn offer(
        &self,
        index: u32,
        mode: u32,
    ) -> Result<Option<OutputOffer>, DecodeFailure> {
        let mut state = self.lock();
        if self.check_running(&mut state).is_err() {
            return Err(state.failure.unwrap_or(DecodeFailure::WrongPhase));
        }
        if mode != EXTRACT_MODE {
            return self.fail(&mut state, DecodeFailure::WrongMode);
        }
        let Some(entry) = self.plan.entries().get(index as usize) else {
            return self.fail(&mut state, DecodeFailure::UnknownIndex);
        };
        if entry.index() != index {
            return self.fail(&mut state, DecodeFailure::UnknownIndex);
        }
        if state.current.is_some() || state.entries[index as usize].phase != EntryPhase::Unseen {
            return self.fail(&mut state, DecodeFailure::DuplicateIndex);
        }
        let token = state.next_token;
        state.next_token = match token.checked_add(1) {
            Some(next) => next,
            None => return self.fail(&mut state, DecodeFailure::InvalidPlan),
        };
        state.entries[index as usize].phase = EntryPhase::Offered { token, mode };
        state.current = Some(CurrentSlot {
            index,
            token,
            mode,
            written: 0,
        });
        Ok(Some(OutputOffer {
            token,
            index,
            range: if entry.kind() == NodeKind::Directory {
                None
            } else {
                entry.spool_range().cloned()
            },
        }))
    }

    /// 現在 offer 中の entry に対する PrepareOperation を受け入れる。
    pub(crate) fn prepare(&self, mode: u32) -> Result<(), DecodeFailure> {
        let mut state = self.lock();
        self.check_running(&mut state)?;
        if mode != EXTRACT_MODE {
            return self.fail(&mut state, DecodeFailure::WrongMode);
        }
        let Some(slot) = state.current.as_ref() else {
            return self.fail(&mut state, DecodeFailure::CallbackOrder);
        };
        if slot.mode != mode
            || state.entries[slot.index as usize].phase
                != (EntryPhase::Offered {
                    token: slot.token,
                    mode: slot.mode,
                })
        {
            return self.fail(&mut state, DecodeFailure::CallbackOrder);
        }
        let index = slot.index as usize;
        let token = slot.token;
        state.entries[index].phase = EntryPhase::Prepared { token, mode };
        Ok(())
    }

    /// 現在の entry range 内へ書く。stream ごとの token で、保持された古い COM stream が
    /// 次の entry を変更できないようにする。
    pub(crate) fn write(
        &self,
        token: u64,
        index: u32,
        bytes: &[u8],
    ) -> Result<usize, DecodeFailure> {
        let mut state = self.lock();
        self.check_running(&mut state)?;
        let Some(slot) = state.current.as_ref() else {
            return self.fail(&mut state, DecodeFailure::CallbackOrder);
        };
        if slot.token != token
            || slot.index != index
            || state.entries.get(index as usize).map(|fact| fact.phase)
                != Some(EntryPhase::Prepared {
                    token,
                    mode: EXTRACT_MODE,
                })
        {
            return self.fail(&mut state, DecodeFailure::CallbackOrder);
        }
        let entry = &self.plan.entries()[index as usize];
        let Some(range) = entry.spool_range() else {
            return self.fail(&mut state, DecodeFailure::RangeExceeded);
        };
        let Some(end) = slot.written.checked_add(bytes.len() as u64) else {
            return self.fail(&mut state, DecodeFailure::RangeExceeded);
        };
        let Some(position) = range.start.checked_add(slot.written) else {
            return self.fail(&mut state, DecodeFailure::RangeExceeded);
        };
        if end > range.end - range.start {
            return self.fail(&mut state, DecodeFailure::RangeExceeded);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if state.spool.seek(SeekFrom::Start(position)).is_err() {
            return self.fail(&mut state, DecodeFailure::Io);
        }
        let written = match state.spool.write(bytes) {
            Ok(count) => count,
            Err(_) => return self.fail(&mut state, DecodeFailure::Io),
        };
        if written == 0 && !bytes.is_empty() {
            return self.fail(&mut state, DecodeFailure::Io);
        }
        if let Some(slot) = state.current.as_mut() {
            slot.written = match slot.written.checked_add(written as u64) {
                Some(next) => next,
                None => return self.fail(&mut state, DecodeFailure::RangeExceeded),
            };
            state.entries[index as usize].written = slot.written;
        }
        Ok(written)
    }

    /// SDK entry を完了する。operation result が非 zero なら失敗し、file は宣言 byte 数と
    /// 実書き込み数が一致しなければ完了しない。
    pub(crate) fn result(&self, operation_result: u32) -> Result<(), DecodeFailure> {
        let mut state = self.lock();
        self.check_running(&mut state)?;
        if operation_result != 0 {
            return self.fail(&mut state, DecodeFailure::InvalidOperationResult);
        }
        let Some(slot) = state.current.as_ref() else {
            return self.fail(&mut state, DecodeFailure::CallbackOrder);
        };
        if state.entries[slot.index as usize].phase
            != (EntryPhase::Prepared {
                token: slot.token,
                mode: slot.mode,
            })
        {
            return self.fail(&mut state, DecodeFailure::CallbackOrder);
        }
        let entry = &self.plan.entries()[slot.index as usize];
        if entry.kind() == NodeKind::File && Some(slot.written) != entry.declared_size() {
            return self.fail(&mut state, DecodeFailure::LengthMismatch);
        }
        let index = slot.index as usize;
        state.entries[index].phase = EntryPhase::Done;
        state.current = None;
        Ok(())
    }

    /// SDK 側 owner を解放する前に callback からの変更を止める。
    pub(crate) fn seal(&self) -> Result<(), DecodeFailure> {
        let mut state = self.lock();
        self.check_cancelled(&mut state)?;
        match state.phase {
            Phase::Running if state.current.is_none() => {
                state.phase = Phase::Sealed;
                Ok(())
            }
            Phase::Running => self.fail(&mut state, DecodeFailure::CallbackOrder),
            _ => Err(state.failure.unwrap_or(DecodeFailure::WrongPhase)),
        }
    }

    /// archive と callback interface を解放した後に行う最終判定。
    pub(crate) fn finish(&self, extract_succeeded: bool) -> Result<(), DecodeFailure> {
        let mut state = self.lock();
        if matches!(
            state.phase,
            Phase::Succeeded | Phase::Failed | Phase::Cancelled
        ) {
            return Err(state.failure.unwrap_or(DecodeFailure::WrongPhase));
        }
        self.check_cancelled(&mut state)?;
        if state.phase != Phase::Sealed {
            return Err(state.failure.unwrap_or(DecodeFailure::WrongPhase));
        }
        let spool_length = match state.spool.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => return self.fail(&mut state, DecodeFailure::Io),
        };
        if !extract_succeeded
            || state
                .entries
                .iter()
                .any(|fact| fact.phase != EntryPhase::Done)
            || state.current.is_some()
            || spool_length != self.plan.total_size()
        {
            return self.fail(&mut state, DecodeFailure::Incomplete);
        }
        self.check_cancelled(&mut state)?;
        state.phase = Phase::Succeeded;
        Ok(())
    }

    /// progress callback から並行して呼べる。entry 成功は決めず、cancel のみ反映する。
    pub(crate) fn check_progress(&self) -> Result<(), DecodeFailure> {
        let mut state = self.lock();
        self.check_cancelled(&mut state)
    }

    pub(crate) fn reject_callback(&self, failure: DecodeFailure) -> DecodeFailure {
        let mut state = self.lock();
        if self.check_cancelled(&mut state).is_err() {
            return state.failure.unwrap_or(DecodeFailure::Cancelled);
        }
        self.fail::<()>(&mut state, failure)
            .err()
            .unwrap_or(failure)
    }

    pub(crate) fn report_extract_result(&self, operation_result: i32) -> Result<(), DecodeFailure> {
        let mut state = self.lock();
        self.check_running(&mut state)?;
        if state.current.is_none()
            && state
                .entries
                .iter()
                .all(|entry| entry.phase == EntryPhase::Done)
            && let Some(observer) = &self.report_observer
        {
            observer.store(true, Ordering::Release);
        }
        if operation_result != 0 {
            return self.fail(&mut state, DecodeFailure::InvalidOperationResult);
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                if !matches!(
                    state.phase,
                    Phase::Succeeded | Phase::Failed | Phase::Cancelled
                ) {
                    state.phase = Phase::Failed;
                    state.failure = Some(DecodeFailure::Poisoned);
                }
                state
            }
        }
    }

    fn check_running(&self, state: &mut State) -> Result<(), DecodeFailure> {
        self.check_cancelled(state)?;
        if state.phase == Phase::Running {
            Ok(())
        } else {
            Err(state.failure.unwrap_or(DecodeFailure::WrongPhase))
        }
    }

    fn check_cancelled(&self, state: &mut State) -> Result<(), DecodeFailure> {
        if matches!(
            state.phase,
            Phase::Succeeded | Phase::Failed | Phase::Cancelled
        ) {
            return Err(state.failure.unwrap_or(DecodeFailure::WrongPhase));
        }
        if self.cancelled.load(Ordering::Acquire) {
            state.phase = Phase::Cancelled;
            state.failure = Some(DecodeFailure::Cancelled);
            state.current = None;
            Err(DecodeFailure::Cancelled)
        } else {
            Ok(())
        }
    }

    fn fail<T>(&self, state: &mut State, failure: DecodeFailure) -> Result<T, DecodeFailure> {
        state.phase = Phase::Failed;
        state.failure = Some(failure);
        state.current = None;
        Err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::seven_zip_extract_plan::IndexedRecord;
    use super::*;
    use std::io::{Read, Seek};

    fn file(index: u32, name: &str, size: u64) -> IndexedRecord {
        IndexedRecord {
            index,
            components: vec![name.to_owned()],
            kind: NodeKind::File,
            declared_size: Some(size),
            crc32: None,
        }
    }

    fn directory(index: u32, name: &str) -> IndexedRecord {
        IndexedRecord {
            index,
            components: vec![name.to_owned()],
            kind: NodeKind::Directory,
            declared_size: None,
            crc32: None,
        }
    }

    fn sized_directory(index: u32, name: &str) -> IndexedRecord {
        IndexedRecord {
            index,
            components: vec![name.to_owned()],
            kind: NodeKind::Directory,
            declared_size: Some(0),
            crc32: None,
        }
    }

    fn invocation(records: Vec<IndexedRecord>) -> (DecodeInvocation, File) {
        let plan = Arc::new(Validated7zExtractPlan::validate(records).unwrap());
        let spool = tempfile::tempfile().unwrap();
        let reader = spool.try_clone().unwrap();
        (
            DecodeInvocation::new(plan, spool, Arc::new(AtomicBool::new(false))).unwrap(),
            reader,
        )
    }

    unsafe fn invoke_message_report(message: *mut c_void, operation_result: i32) -> i32 {
        let message = message.cast::<ExtractMessage>();
        ((*(*message).vtable).report_extract_result)(message, 0, 0, operation_result)
    }

    #[test]
    fn writes_directory_empty_and_multichunk_files_to_exact_ranges() {
        let (decode, mut reader) = invocation(vec![
            directory(0, "dir"),
            file(1, "empty", 0),
            file(2, "data", 5),
        ]);
        for index in 0..3 {
            let offer = decode.offer(index, EXTRACT_MODE).unwrap().unwrap();
            assert_eq!(offer.range.is_none(), index == 0);
            decode.prepare(EXTRACT_MODE).unwrap();
            if index == 2 {
                assert_eq!(decode.write(offer.token, index, b"ab").unwrap(), 2);
                assert_eq!(decode.write(offer.token, index, b"cde").unwrap(), 3);
            }
            decode.result(0).unwrap();
        }
        decode.seal().unwrap();
        decode.finish(true).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
        assert_eq!(bytes, b"abcde");
    }

    #[test]
    fn overflow_and_short_result_fail_stickily() {
        let (decode, _) = invocation(vec![file(0, "f", 2)]);
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        assert_eq!(
            decode.write(offer.token, 0, b"abc"),
            Err(DecodeFailure::RangeExceeded)
        );
        assert_eq!(decode.result(0), Err(DecodeFailure::RangeExceeded));

        let (decode, _) = invocation(vec![file(0, "f", 2)]);
        decode.offer(0, EXTRACT_MODE).unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        assert_eq!(decode.result(0), Err(DecodeFailure::LengthMismatch));
    }

    #[test]
    fn rejects_missing_duplicate_unknown_wrong_mode_and_reordered_phases() {
        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        assert_eq!(
            decode.offer(3, EXTRACT_MODE),
            Err(DecodeFailure::UnknownIndex)
        );

        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        assert_eq!(decode.offer(0, 1), Err(DecodeFailure::WrongMode));

        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        assert_eq!(
            decode.prepare(EXTRACT_MODE),
            Err(DecodeFailure::CallbackOrder)
        );

        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        decode.offer(0, EXTRACT_MODE).unwrap();
        assert_eq!(
            decode.offer(0, EXTRACT_MODE),
            Err(DecodeFailure::DuplicateIndex)
        );

        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        decode.offer(0, EXTRACT_MODE).unwrap();
        assert_eq!(decode.result(0), Err(DecodeFailure::CallbackOrder));

        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        decode.offer(0, EXTRACT_MODE).unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        assert_eq!(decode.finish(true), Err(DecodeFailure::WrongPhase));
    }

    #[test]
    fn accepts_out_of_order_indices_and_writes_each_planned_range() {
        let (decode, mut reader) = invocation(vec![file(0, "a", 2), file(1, "b", 2)]);
        let second = decode.offer(1, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.write(second.token, 1, b"cd").unwrap();
        decode.result(0).unwrap();
        let first = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.write(first.token, 0, b"ab").unwrap();
        decode.result(0).unwrap();
        decode.seal().unwrap();
        decode.finish(true).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
        assert_eq!(bytes, b"abcd");
    }

    #[test]
    fn result_invalidates_retained_stream_token_and_next_slot() {
        let (decode, _) = invocation(vec![file(0, "a", 1), file(1, "b", 1)]);
        let first = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.write(first.token, 0, b"x").unwrap();
        decode.result(0).unwrap();
        assert_eq!(
            decode.write(first.token, 0, b""),
            Err(DecodeFailure::CallbackOrder)
        );

        let (decode, _) = invocation(vec![file(0, "a", 1), file(1, "b", 1)]);
        let first = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.write(first.token, 0, b"x").unwrap();
        decode.result(0).unwrap();
        let second = decode.offer(1, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        assert_eq!(
            decode.write(first.token, 0, b""),
            Err(DecodeFailure::CallbackOrder)
        );
        assert_eq!(
            decode.write(second.token, 1, b"y"),
            Err(DecodeFailure::CallbackOrder),
            "a stale stream callback permanently fails this invocation"
        );
        assert_eq!(decode.finish(true), Err(DecodeFailure::CallbackOrder));
        let mut bytes = Vec::new();
        {
            let mut state = decode.lock();
            state.spool.seek(SeekFrom::Start(0)).unwrap();
            state.spool.read_to_end(&mut bytes).unwrap();
        }
        assert_eq!(bytes, b"x");
    }

    #[test]
    fn nonzero_result_and_cancellation_are_terminal_without_mutation() {
        let (decode, mut reader) = invocation(vec![file(0, "f", 1)]);
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        assert_eq!(decode.result(1), Err(DecodeFailure::InvalidOperationResult));
        assert_eq!(
            decode.write(offer.token, 0, b"x"),
            Err(DecodeFailure::InvalidOperationResult)
        );
        reader.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(reader.stream_position().unwrap(), 0);

        let plan = Arc::new(Validated7zExtractPlan::validate(vec![file(0, "f", 1)]).unwrap());
        let spool = tempfile::tempfile().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let decode = DecodeInvocation::new(plan, spool, cancel.clone()).unwrap();
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        cancel.store(true, Ordering::Release);
        assert_eq!(
            decode.write(offer.token, 0, b"x"),
            Err(DecodeFailure::Cancelled)
        );
        assert_eq!(decode.finish(true), Err(DecodeFailure::Cancelled));
    }

    #[test]
    fn rejects_nonempty_spools_without_truncating() {
        let plan = Arc::new(Validated7zExtractPlan::validate(vec![]).unwrap());
        let mut spool = tempfile::tempfile().unwrap();
        spool.write_all(b"keep").unwrap();
        assert!(matches!(
            DecodeInvocation::new(
                plan,
                spool.try_clone().unwrap(),
                Arc::new(AtomicBool::new(false))
            ),
            Err(DecodeFailure::InvalidSpool)
        ));
        spool.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut spool, &mut bytes).unwrap();
        assert_eq!(bytes, b"keep");
    }

    #[test]
    fn rejects_an_actual_directory_handle_as_spool() {
        use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
        use windows::{
            Win32::{
                Foundation::{GENERIC_READ, GENERIC_WRITE},
                Storage::FileSystem::{
                    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
                    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
                },
            },
            core::PCWSTR,
        };

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("spool-directory");
        std::fs::create_dir(&directory).unwrap();
        let wide = directory
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .unwrap();
        let directory_handle = unsafe { File::from_raw_handle(handle.0) };
        assert!(directory_handle.metadata().unwrap().is_dir());
        let plan = Arc::new(Validated7zExtractPlan::validate(vec![]).unwrap());
        assert!(matches!(
            DecodeInvocation::new(plan, directory_handle, Arc::new(AtomicBool::new(false))),
            Err(DecodeFailure::InvalidSpool)
        ));
    }

    #[test]
    fn cancellation_between_seal_and_final_gate_rejects_completion() {
        let (decode, _) = invocation(vec![file(0, "f", 1)]);
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.write(offer.token, 0, b"x").unwrap();
        decode.result(0).unwrap();
        decode.seal().unwrap();
        decode.cancelled.store(true, Ordering::Release);
        assert_eq!(decode.finish(true), Err(DecodeFailure::Cancelled));
    }

    #[test]
    fn cancellation_is_observed_after_waiting_for_state_mutex() {
        use std::{sync::mpsc, thread};

        let (decode, _) = invocation(vec![file(0, "f", 0)]);
        let decode = Arc::new(decode);
        let state_guard = decode.lock();
        let worker_decode = decode.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(worker_decode.check_progress()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            result_rx.try_recv().is_err(),
            "the held mutex blocks progress admission"
        );
        decode.cancelled.store(true, Ordering::Release);
        drop(state_guard);
        assert_eq!(result_rx.recv().unwrap(), Err(DecodeFailure::Cancelled));
        worker.join().unwrap();
    }

    #[test]
    fn concurrent_progress_observes_writes_without_deciding_entry_result() {
        use std::sync::{Barrier, mpsc};
        use std::thread;

        let (decode, mut reader) = invocation(vec![file(0, "f", 2)]);
        let decode = Arc::new(decode);
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let progress_decode = decode.clone();
        let progress_barrier = barrier.clone();
        let (tx, rx) = mpsc::channel();
        let progress = thread::spawn(move || {
            progress_barrier.wait();
            tx.send(progress_decode.check_progress()).unwrap();
        });
        barrier.wait();
        assert_eq!(decode.write(offer.token, 0, b"a").unwrap(), 1);
        assert_eq!(rx.recv().unwrap(), Ok(()));
        assert_eq!(decode.write(offer.token, 0, b"b").unwrap(), 1);
        progress.join().unwrap();
        decode.result(0).unwrap();
        decode.seal().unwrap();
        decode.finish(true).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"ab");
    }

    #[test]
    fn accepts_zero_sized_directory_and_keeps_success_terminal_after_late_cancel() {
        let (decode, _) = invocation(vec![sized_directory(0, "dir")]);
        decode.offer(0, EXTRACT_MODE).unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.result(0).unwrap();
        decode.seal().unwrap();
        decode.finish(true).unwrap();
        decode.cancelled.store(true, Ordering::Release);
        assert_eq!(decode.check_progress(), Err(DecodeFailure::WrongPhase));
        assert_eq!(decode.finish(false), Err(DecodeFailure::WrongPhase));
    }

    #[test]
    fn callback_and_stream_com_references_survive_owner_release() {
        let plan = Arc::new(Validated7zExtractPlan::validate(vec![file(0, "f", 1)]).unwrap());
        let spool = tempfile::tempfile().unwrap();
        let invocation =
            Arc::new(DecodeInvocation::new(plan, spool, Arc::new(AtomicBool::new(false))).unwrap());

        let owner = new_callback_owner(invocation.clone());
        let mut retained_callback = ptr::null_mut();
        unsafe {
            assert_eq!(
                extract_query_interface(owner.0.cast(), &IID_I_PROGRESS, &mut retained_callback,),
                S_OK.0
            );
            drop(owner);
            assert_eq!(progress_set_total(retained_callback.cast(), 1), S_OK.0);
            assert_eq!(extract_release(retained_callback.cast()), 0);
        }

        let offer = invocation.offer(0, EXTRACT_MODE).unwrap().unwrap();
        invocation.prepare(EXTRACT_MODE).unwrap();
        let stream = Box::new(OutputStream {
            vtable: &OUTPUT_VTABLE,
            refs: AtomicU32::new(1),
            invocation: invocation.clone(),
            token: offer.token,
            index: 0,
        });
        let stream = Box::into_raw(stream);
        let mut retained_stream = ptr::null_mut();
        unsafe {
            assert_eq!(
                output_query_interface(
                    stream.cast(),
                    &IID_I_SEQUENTIAL_OUT_STREAM,
                    &mut retained_stream,
                ),
                S_OK.0
            );
            assert_eq!(output_release(stream.cast()), 1);
            let byte = [b'x'];
            assert_eq!(
                output_write(
                    retained_stream.cast(),
                    byte.as_ptr().cast(),
                    1,
                    ptr::null_mut()
                ),
                S_OK.0
            );
            assert_eq!(output_release(retained_stream.cast()), 0);
        }
        invocation.result(0).unwrap();
        invocation.seal().unwrap();
        invocation.finish(true).unwrap();
    }

    #[test]
    fn extract_message_query_identity_and_reference_lifetime_are_shared() {
        let (decode, _) = invocation(vec![file(0, "empty", 0)]);
        let decode = Arc::new(decode);
        let owner = new_callback_owner(decode.clone());
        let primary = owner.0.cast::<Unknown>();
        let mut raw_message = ptr::null_mut();
        unsafe {
            assert_eq!(
                extract_query_interface(
                    primary,
                    &IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2,
                    &mut raw_message,
                ),
                S_OK.0
            );
            assert!(!raw_message.is_null());
            let mut canonical_unknown = ptr::null_mut();
            assert_eq!(
                message_query_interface(raw_message.cast(), &IID_IUNKNOWN, &mut canonical_unknown),
                S_OK.0
            );
            assert_eq!(canonical_unknown, primary.cast());
            assert_eq!(extract_release(canonical_unknown.cast()), 2);
        }
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.result(0).unwrap();
        drop(owner);
        unsafe {
            assert_eq!(
                invoke_message_report(raw_message, 0),
                S_OK.0,
                "the secondary COM interface keeps its common owner alive"
            );
            assert_eq!(message_release(raw_message.cast()), 0);
        }
        decode.seal().unwrap();
        decode.finish(true).unwrap();
        assert_eq!(offer.index, 0);
    }

    #[test]
    fn extract_message_error_after_all_entries_done_is_terminal() {
        let (decode, mut reader) = invocation(vec![file(0, "f", 1)]);
        let decode = Arc::new(decode);
        let owner = new_callback_owner(decode.clone());
        let mut raw_message = ptr::null_mut();
        unsafe {
            assert_eq!(
                extract_query_interface(
                    owner.0.cast(),
                    &IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2,
                    &mut raw_message,
                ),
                S_OK.0
            );
        }
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        decode.write(offer.token, 0, b"x").unwrap();
        decode.result(0).unwrap();
        unsafe {
            assert_eq!(
                invoke_message_report(raw_message, 1),
                E_FAIL,
                "a late archive-level failure must override completed entry callbacks"
            );
            assert_eq!(message_release(raw_message.cast()), 1);
        }
        assert_eq!(
            decode.check_progress(),
            Err(DecodeFailure::InvalidOperationResult)
        );
        assert_eq!(decode.seal(), Err(DecodeFailure::InvalidOperationResult));
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"x");
    }

    #[test]
    fn successful_extract_message_report_does_not_mark_unseen_entries_done() {
        let (decode, _) = invocation(vec![file(0, "f", 1)]);
        assert_eq!(decode.report_extract_result(0), Ok(()));
        decode.seal().unwrap();
        assert_eq!(decode.finish(true), Err(DecodeFailure::Incomplete));
    }

    #[test]
    fn successful_extract_message_report_returns_abort_when_cancelled() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let plan = Arc::new(Validated7zExtractPlan::validate(vec![file(0, "f", 0)]).unwrap());
        let decode = Arc::new(
            DecodeInvocation::new(plan, tempfile::tempfile().unwrap(), cancelled.clone()).unwrap(),
        );
        let owner = new_callback_owner(decode);
        let mut message = ptr::null_mut();
        unsafe {
            assert_eq!(
                extract_query_interface(
                    owner.0.cast(),
                    &IID_I_ARCHIVE_EXTRACT_CALLBACK_MESSAGE2,
                    &mut message,
                ),
                S_OK.0
            );
        }
        cancelled.store(true, Ordering::Release);
        unsafe {
            assert_eq!(invoke_message_report(message, 0), E_ABORT);
            assert_eq!(message_release(message.cast()), 1);
        }
    }

    #[test]
    fn read_only_spool_write_failure_is_sticky() {
        use std::fs::OpenOptions;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("readonly-spool");
        std::fs::write(&path, b"").unwrap();
        let spool = OpenOptions::new().read(true).open(&path).unwrap();
        let plan = Arc::new(Validated7zExtractPlan::validate(vec![file(0, "f", 1)]).unwrap());
        let decode = DecodeInvocation::new(plan, spool, Arc::new(AtomicBool::new(false))).unwrap();
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        assert_eq!(decode.write(offer.token, 0, b"x"), Err(DecodeFailure::Io));
        assert_eq!(decode.write(offer.token, 0, b"x"), Err(DecodeFailure::Io));
        assert_eq!(decode.result(0), Err(DecodeFailure::Io));
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }

    #[test]
    fn com_zero_write_accepts_null_data_and_retained_stream_fails_after_result() {
        use std::io::{Read, Seek, SeekFrom};

        let plan = Arc::new(Validated7zExtractPlan::validate(vec![file(0, "empty", 0)]).unwrap());
        let spool = tempfile::tempfile().unwrap();
        let mut reader = spool.try_clone().unwrap();
        let invocation =
            Arc::new(DecodeInvocation::new(plan, spool, Arc::new(AtomicBool::new(false))).unwrap());
        let owner = new_callback_owner(invocation.clone());
        let mut raw_stream = ptr::null_mut();
        unsafe {
            assert_eq!(
                extract_get_stream(owner.0, 0, &mut raw_stream, EXTRACT_MODE),
                S_OK.0
            );
            assert!(!raw_stream.is_null());
            assert_eq!(extract_prepare(owner.0, EXTRACT_MODE), S_OK.0);
            assert_eq!(
                output_write(raw_stream.cast(), ptr::null(), 0, ptr::null_mut()),
                S_OK.0
            );
            assert_eq!(extract_set_result(owner.0, 0), S_OK.0);
            assert_eq!(invocation.seal(), Ok(()));
            assert_eq!(
                output_write(raw_stream.cast(), ptr::null(), 0, ptr::null_mut()),
                E_FAIL
            );
            output_release(raw_stream.cast());
        }
        invocation.finish(true).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert!(
            bytes.is_empty(),
            "late retained stream write after seal cannot mutate spool"
        );
    }

    #[test]
    fn malformed_callback_pointers_fail_stickily_but_unsupported_qi_is_harmless() {
        let (decode, _) = invocation(vec![file(0, "f", 1)]);
        let owner = new_callback_owner(Arc::new(decode));
        let mut unsupported = ptr::null_mut();
        unsafe {
            assert_eq!(
                extract_query_interface(
                    owner.0.cast(),
                    &IID_I_SEQUENTIAL_OUT_STREAM,
                    &mut unsupported,
                ),
                0x8000_4002u32 as i32
            );
            assert_eq!((*owner.0).invocation.check_progress(), Ok(()));
            assert_eq!(
                extract_get_stream(owner.0, 0, ptr::null_mut(), EXTRACT_MODE),
                E_INVALIDARG
            );
            assert_eq!(
                (*owner.0).invocation.check_progress(),
                Err(DecodeFailure::InvalidArgument)
            );
        }

        let (decode, _) = invocation(vec![file(0, "f", 1)]);
        let offer = decode.offer(0, EXTRACT_MODE).unwrap().unwrap();
        decode.prepare(EXTRACT_MODE).unwrap();
        let stream = Box::new(OutputStream {
            vtable: &OUTPUT_VTABLE,
            refs: AtomicU32::new(1),
            invocation: Arc::new(decode),
            token: offer.token,
            index: 0,
        });
        let stream = Box::into_raw(stream);
        unsafe {
            assert_eq!(
                output_write(stream, ptr::null(), 1, ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(
                (*stream).invocation.check_progress(),
                Err(DecodeFailure::InvalidArgument)
            );
            assert_eq!(output_release(stream.cast()), 0);
        }
    }
}
